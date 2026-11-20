//! `flow_pay` and `flow_read_back`: the one-time Payment Request flow (Paykit spec 8.1)
//! against the real Switch service behind the rig, on the run's stores, and reading it back
//! from those stores after a reload.
//!
//! The steps and checks are those of `test_switch_flow_request_paid_proven_and_receipted` in
//! `tests/switch_flow/one_time.rs`, in the same world (`tests/switch_flow/switch_world.rs`),
//! with each Paykit App on a store of its own (`stores.rs`).

use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::timer::JsTimer;
use paykit_sdk::{
    MessageDelivery, PayOptions, PayOutcome, PayeeActivity, PayeeStatus, Paykit, PaykitConfig,
    ReceiptsOf, RequestRef, RequestStatus,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use pubky_switch_client::testing::TestHomeserver;
use pubky_switch_core::enc::b64;
use serde_json::json;

use crate::failure::{delivery_kind, ensure, outcome_kind, Args, Failure, Outcome};
use crate::stores::{self, StoreKind, APPS};
use crate::switch_rig::wait::now_ms;
use crate::switch_rig::{transport, try_rig_token};
use crate::switch_world::{app_env, js_rail, SwitchWorld, PAYEE, PAYER};
use crate::world::{draft, ASSET, DAY_MS, PAYMENT_REFERENCE};

/// The amount of the Payment Request, in btc.
const AMOUNT: &str = "0.00025";

/// `flow_pay`, with the arguments `run`, `store` (`origin`, `host` or `memory`) and
/// `switch`, the rig's origin.
///
/// Opens the run's two stores, asks the rig for a world with a payer and a payee, and joins
/// the payer's wallet and the payee's shop to the service through `FetchTransport`, each on
/// its store, keeping each one's App ID in its store. The wallet invites the payee; the shop
/// sends a one-time Payment Request, which the service accepts; the wallet receives it once
/// and reads it open, as sent, and pays it through the fake rail; the shop receives the
/// Payment Proof and confirms it with a Payment Receipt; and the wallet stores the receipt
/// with the request. Every store is closed when it returns.
///
/// Returns `{"store", "payee_id", "request_id", "receipt_id", "amount", "asset",
/// "request_state", "payee_state", "timings": {"join", "request", "pay", "receipt",
/// "total"}}`, timings in milliseconds. The request is named by its payee's identity
/// (`payee_id`) and its request hash (`request_id`), both in base64url (Switch wire 1.2), and
/// the Payment Receipt by its `msg_id` (`receipt_id`), as `flow_read_back` takes them back.
pub async fn pay(args: &Args) -> Outcome {
    let started = now_ms();
    args.check_switch()?;
    try_rig_token().map_err(Failure::build)?;
    let run = args.run()?;
    let kind = StoreKind::from_args(args)?;

    let wallet_store = stores::open(kind, run, APPS[0]).await?;
    let shop_store = stores::open(kind, run, APPS[1]).await?;
    for (store, app) in [(&wallet_store, APPS[0]), (&shop_store, APPS[1])] {
        if stores::app_id(store.as_ref(), app)?.is_some() {
            return Err(Failure::bad_args(format!(
                "the store of the {app} of this run holds an app already: use a new run"
            )));
        }
    }

    // The world and both apps, on the service.
    let joining = now_ms();
    let w = SwitchWorld::try_enrol(&[PAYER, PAYEE], &[(PAYER, APPS[0]), (PAYEE, APPS[1])])
        .await
        // The rig's text may quote its answer or a transport error: only the step is kept.
        .map_err(|_| Failure::rig("the rig enrols a world with the payer and the payee"))?;
    let mut wallet = join(&w, PAYER, APPS[0], &wallet_store).await?;
    let mut shop = join(&w, PAYEE, APPS[1], &shop_store).await?;
    wallet
        .app_mut()
        .invite(&w.id(PAYEE))
        .await
        .map_err(|error| Failure::sdk("the wallet invites the payee", &error.into()))?;
    let join_ms = now_ms() - joining;

    // The payee's shop sends a one-time Payment Request, which the payer's switch accepts at
    // once, and the payer's wallet receives it, once, and shows it open, as the payee sent
    // it.
    let requesting = now_ms();
    let sent_draft = draft(AMOUNT, JsClock.now_ms() + DAY_MS);
    let sent = shop
        .send_request(w.id(PAYER), sent_draft.clone())
        .await
        .map_err(|error| Failure::sdk("the shop sends the request", &error))?;
    ensure!(
        matches!(sent.delivery, MessageDelivery::Accepted { .. }),
        "the service accepts the request at once, not {}",
        delivery_kind(&sent.delivery)
    );
    let request = sent
        .request
        .ok_or_else(|| Failure::assertion("the shop records the request"))?;
    let summary = wallet
        .receive(None)
        .await
        .map_err(|error| Failure::sdk("the wallet receives the request", &error))?;
    ensure!(
        summary.applied == [sent.msg_id.clone()],
        "the wallet applies the request once, not {} messages",
        summary.applied.len()
    );
    let view = request_view(&wallet, &request).await?;
    ensure!(
        view.status == RequestStatus::Open,
        "the request reads open, not {:?}",
        view.status
    );
    ensure!(view.request == request, "the view is of the request sent");
    ensure!(
        view.amount == sent_draft.amount,
        "the amount is the one sent"
    );
    ensure!(
        view.payment_reference == sent_draft.payment_reference.as_str(),
        "the Payment Reference is the one sent"
    );
    ensure!(
        view.proposal_expires_at_ms == sent_draft.proposal_expires_at_ms,
        "the expiry is the one sent"
    );
    let request_ms = now_ms() - requesting;

    // The payer's wallet pays: attempt, rail, acceptance and proof.
    let paying = now_ms();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .map_err(|error| Failure::sdk("the wallet pays", &error))?;
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        return Err(Failure::assertion(format!(
            "the payment is paid, not {}",
            outcome_kind(&outcome)
        )));
    };
    ensure!(
        w.rail.executions(&attempt_id) == 1,
        "the rail executes the payment once"
    );
    let pay_ms = now_ms() - paying;

    // The payee's shop receives the acceptance and the Payment Proof, verifies the proof and
    // sends a Payment Receipt, which the payer's wallet stores with the request it pays.
    let receipting = now_ms();
    shop.receive(None)
        .await
        .map_err(|error| Failure::sdk("the shop receives the proof", &error))?;
    let sale = only_sale(&shop).await?;
    ensure!(
        sale.status == PayeeStatus::ProofReceived,
        "the shop holds the Payment Proof, not {:?}",
        sale.status
    );
    ensure!(
        sale.attempt_id == Some(attempt_id),
        "the proof is of the attempt paid"
    );
    let record = sale.record;
    let receipt_msg_id = shop
        .confirm_payment(&record)
        .await
        .map_err(|error| Failure::sdk("the shop confirms", &error))?
        .ok_or_else(|| Failure::assertion("the proof verifies, so a receipt is sent"))?;
    let payee_state = only_sale(&shop).await?.status;
    ensure!(
        payee_state == PayeeStatus::Receipted,
        "the shop's sale reads receipted, not {payee_state:?}"
    );
    wallet
        .receive(None)
        .await
        .map_err(|error| Failure::sdk("the wallet receives the receipt", &error))?;
    let view = request_view(&wallet, &request).await?;
    check_receipted(&wallet, &view, &request, &receipt_msg_id, w.id(PAYEE)).await?;
    ensure!(
        view.receipt.as_ref().map(|receipt| receipt.attempt_id) == Some(attempt_id),
        "the receipt confirms the attempt paid"
    );
    let receipt_ms = now_ms() - receipting;

    // The host remembers which app keeps its state in each store, for the read-back.
    stores::keep_app_id(wallet_store.as_ref(), APPS[0], wallet.app_id())?;
    stores::keep_app_id(shop_store.as_ref(), APPS[1], shop.app_id())?;
    let status = view.status;
    // Closes both stores: the apps hold the last handles but these.
    drop((wallet, shop, w));
    drop((wallet_store, shop_store));

    Ok(json!({
        "store": kind.as_str(),
        "payee_id": b64(&request.payee),
        "request_id": b64(&request.request_hash),
        "receipt_id": receipt_msg_id,
        "amount": AMOUNT,
        "asset": ASSET,
        "request_state": format!("{status:?}"),
        "payee_state": format!("{payee_state:?}"),
        "timings": {
            "join": join_ms,
            "request": request_ms,
            "pay": pay_ms,
            "receipt": receipt_ms,
            "total": now_ms() - started,
        },
    }))
}

/// `flow_read_back`, with the arguments `run`, `store` (`origin` or `host`, as `flow_pay` was
/// given), `switch`, the rig's origin, and what `flow_pay` returned: `payee_id`,
/// `request_id` and `receipt_id`.
///
/// In a new worker, after a reload: opens the run's two stores again, reads from each the App
/// ID of its app, loads both apps from their stores alone with `Paykit::load` (nothing is
/// sent), and checks what the stores hold: the request reads paid, with the Payment Receipt
/// `flow_pay` saw, from the payee, for the request, with its Payment Reference; the wallet
/// lists that one receipt; and the shop's one sale reads receipted. Every store is closed when
/// it returns.
///
/// Returns `{"store", "request_state", "payee_state", "receipt_id", "receipts", "amount",
/// "asset", "timings": {"open", "load", "total"}}`, timings in milliseconds.
pub async fn read_back(args: &Args) -> Outcome {
    let started = now_ms();
    args.check_switch()?;
    let run = args.run()?;
    let kind = StoreKind::from_args(args)?;
    if kind == StoreKind::Memory {
        return Err(Failure::bad_args(
            "a memory store does not outlive its worker: there is nothing to read back",
        ));
    }
    let request = RequestRef {
        payee: args.bytes("payee_id")?,
        request_hash: args.bytes("request_id")?,
    };
    let receipt_msg_id = args.text("receipt_id")?;

    let opening = now_ms();
    let wallet_store = stores::open(kind, run, APPS[0]).await?;
    let shop_store = stores::open(kind, run, APPS[1]).await?;
    let open_ms = now_ms() - opening;

    let loading = now_ms();
    let wallet = load(&wallet_store, APPS[0]).await?;
    let shop = load(&shop_store, APPS[1]).await?;
    let load_ms = now_ms() - loading;

    ensure!(
        shop.identity() == request.payee,
        "the shop's identity is the request's payee"
    );
    let view = request_view(&wallet, &request).await?;
    check_receipted(&wallet, &view, &request, receipt_msg_id, shop.identity()).await?;
    ensure!(view.amount.value.as_str() == AMOUNT, "the amount read back");
    ensure!(view.amount.asset.as_str() == ASSET, "the asset read back");
    let payee_state = only_sale(&shop).await?.status;
    ensure!(
        payee_state == PayeeStatus::Receipted,
        "the shop's sale reads receipted, not {payee_state:?}"
    );
    let status = view.status;
    drop((wallet, shop));
    drop((wallet_store, shop_store));

    Ok(json!({
        "store": kind.as_str(),
        "request_state": format!("{status:?}"),
        "payee_state": format!("{payee_state:?}"),
        "receipt_id": receipt_msg_id,
        "receipts": 1,
        "amount": AMOUNT,
        "asset": ASSET,
        "timings": {
            "open": open_ms,
            "load": load_ms,
            "total": now_ms() - started,
        },
    }))
}

/// Joins `app` of `identity` in `w` on `store`, through the rig's transport.
async fn join(
    w: &SwitchWorld,
    identity: &str,
    app: &str,
    store: &Arc<dyn StateStore>,
) -> Result<Paykit, Failure> {
    w.try_join_on(
        &format!("{identity}:{app}"),
        Arc::new(transport()),
        Arc::clone(store),
    )
    .await
    // The world's text may quote an SDK error: only the step is kept.
    .map_err(|_| Failure::switch(format!("the {app} of the {identity} joins")))
}

/// Loads the app whose App ID `store` holds, from `store` alone, as a host does after a
/// restart: the rig's transport, the browser's clock and timers, a fresh fake rail, and an
/// empty in-memory Homeserver, as loading sends and reads nothing.
async fn load(store: &Arc<dyn StateStore>, app: &str) -> Result<Paykit, Failure> {
    let app_id = stores::app_id(store.as_ref(), app)?
        .ok_or_else(|| Failure::assertion(format!("the store of the {app} holds its App ID")))?;
    let env = app_env(
        Arc::clone(store),
        Arc::new(transport()),
        &Arc::new(TestHomeserver::new()),
    );
    Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        js_rail(),
        Arc::new(JsTimer),
    )
    .await
    .map_err(|error| Failure::sdk(&format!("load the {app} from its store"), &error))
}

/// The wallet's view of `request`.
async fn request_view(
    wallet: &Paykit,
    request: &RequestRef,
) -> Result<paykit_sdk::RequestView, Failure> {
    wallet
        .request(request)
        .await
        .map_err(|error| Failure::sdk("the wallet reads the request", &error))
}

/// The shop's one sale.
async fn only_sale(shop: &Paykit) -> Result<PayeeActivity, Failure> {
    let sales = shop
        .payee_activity()
        .await
        .map_err(|error| Failure::sdk("the shop reads its sales", &error))?;
    let [sale] = <[PayeeActivity; 1]>::try_from(sales).map_err(|sales| {
        Failure::assertion(format!("the shop sees one sale, not {}", sales.len()))
    })?;
    Ok(sale)
}

/// Checks that `view`, the wallet's view of `request`, reads paid with the Payment Receipt
/// `receipt_msg_id` from `payee`, for the request, with its Payment Reference, and that the
/// wallet lists that one receipt.
async fn check_receipted(
    wallet: &Paykit,
    view: &paykit_sdk::RequestView,
    request: &RequestRef,
    receipt_msg_id: &str,
    payee: [u8; 32],
) -> Result<(), Failure> {
    ensure!(
        view.status == RequestStatus::Paid,
        "the request reads paid, not {:?}",
        view.status
    );
    let receipt = view
        .receipt
        .as_ref()
        .ok_or_else(|| Failure::assertion("the wallet stores the receipt"))?;
    ensure!(
        receipt.msg_id == receipt_msg_id,
        "the receipt is the one the shop sent"
    );
    ensure!(receipt.payee == payee, "the receipt is from the payee");
    ensure!(
        receipt.request_hash == Some(request.request_hash),
        "the receipt is for the request"
    );
    ensure!(
        receipt.payment_reference.as_deref() == Some(PAYMENT_REFERENCE),
        "the receipt carries the request's Payment Reference"
    );
    let listed = wallet
        .receipts(ReceiptsOf::All)
        .await
        .map_err(|error| Failure::sdk("the wallet lists its receipts", &error))?;
    ensure!(
        listed.len() == 1 && listed[0].msg_id == receipt_msg_id,
        "the wallet lists the one receipt, not {} receipts",
        listed.len()
    );
    Ok(())
}
