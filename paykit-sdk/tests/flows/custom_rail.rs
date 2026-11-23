//! A rail of a method `paykit-lib` doesn't know (Paykit wire 1.5): its proofs name their
//! settlement in a spelling of its own, which the rail reads ([`Rail::settlement_id`]), so
//! a payee credits a payment without a request through it, once per settlement.

mod restore;

use std::sync::{Arc, Mutex};

use paykit_lib::bodies::PaymentProof;
use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_sdk::rail::{
    EndpointSelection, FakeRail, PaymentEndpointCandidate, ProofCheck, RailDeclaration, RailError,
    RailOutcome, RailPayment, Transfer,
};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, RequestedScopes,
    UnrequestedPayment,
};
use pubky_switch_client::testing::TestSigner;
use pubky_switch_client::transport::BoxFuture;
use serde_json::{json, Map, Value};

use crate::common::{btc, endpoint, list_version, store, World};

/// The fake rail, as a ledger whose proofs read `{ "type": "x-ledger", "entry": ... }`
/// rather than the `bitcoin-txid` spelling `paykit-lib` knows.
struct LedgerRail {
    fake: Arc<FakeRail>,
    last: Mutex<Option<Map<String, Value>>>,
    supplies_id: bool,
    /// The most the ledger may charge on top of a payment.
    fee: Option<paykit_lib::Decimal>,
}

fn to_ledger(proof: &Map<String, Value>) -> Map<String, Value> {
    json!({ "type": "x-ledger", "entry": proof["txid"] })
        .as_object()
        .cloned()
        .unwrap()
}

fn to_fake(proof: &Map<String, Value>) -> Map<String, Value> {
    json!({ "type": "bitcoin-txid", "txid": proof["entry"] })
        .as_object()
        .cloned()
        .unwrap()
}

impl LedgerRail {
    fn outcome(&self, outcome: RailOutcome) -> RailOutcome {
        match outcome {
            RailOutcome::Paid(mut paid) => {
                paid.proof = to_ledger(&paid.proof);
                *self.last.lock().unwrap() = Some(paid.proof.clone());
                RailOutcome::Paid(paid)
            }
            other => other,
        }
    }
}

impl Rail for LedgerRail {
    fn declaration(&self) -> RailDeclaration {
        self.fake.declaration()
    }

    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        amount: Option<&PaymentAmount>,
    ) -> Result<EndpointSelection, RailError> {
        self.fake.select_endpoints(candidates, amount)
    }

    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        self.fake.endpoint_identifiers()
    }

    fn transfer(
        &self,
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        Transfer::new(
            self.fake.transfer(endpoint_id, payload, amount).amount,
            self.fee.clone(),
        )
    }

    fn fraction_digits(&self, endpoint_id: &PaymentEndpointIdentifier) -> Option<u32> {
        self.fake.fraction_digits(endpoint_id)
    }

    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        Box::pin(async move { self.fake.pay(payment).await.map(|o| self.outcome(o)) })
    }

    fn payment(
        &self,
        idempotency_key: AttemptId,
    ) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>> {
        Box::pin(async move {
            Ok(self
                .fake
                .payment(idempotency_key)
                .await?
                .map(|o| self.outcome(o)))
        })
    }

    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        let mut check = check;
        check.proof = to_fake(&check.proof);
        Box::pin(async move { self.fake.verify(check).await })
    }

    /// The ledger's entry names its settlement.
    fn settlement_id(&self, proof: &Map<String, Value>) -> Option<Vec<u8>> {
        if !self.supplies_id {
            return None;
        }
        (proof.get("type")?.as_str()? == "x-ledger")
            .then(|| proof.get("entry")?.as_str().map(|e| e.as_bytes().to_vec()))
            .flatten()
    }
}

async fn joined(
    w: &World,
    signer: &TestSigner,
    name: &str,
    scopes: RequestedScopes,
    rail: &Arc<LedgerRail>,
) -> Paykit {
    joined_with(w, signer, name, scopes, rail, PaykitConfig::default()).await
}

/// [`joined`], with `config`.
async fn joined_with(
    w: &World,
    signer: &TestSigner,
    name: &str,
    scopes: RequestedScopes,
    rail: &Arc<LedgerRail>,
    config: PaykitConfig,
) -> Paykit {
    let mut app = Paykit::join(
        w.env(signer, store()),
        config,
        w.approve(signer, name, &scopes),
        Arc::clone(rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    app
}

/// A proof alice's wallet sends bob's shop for its list's reusable endpoint, `proof`.
fn proof_body(
    shop: [u8; 16],
    version: paykit_lib::ListVersion,
    proof: Map<String, Value>,
) -> Map<String, Value> {
    PaymentProof {
        attempt_id: AttemptId::generate().unwrap(),
        billing_period: None,
        payment_endpoint_identifier: endpoint("btc-lightning-bolt12"),
        payment_app_id: shop,
        list_version: Some(version),
        amount: Some(btc("0.0015")),
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof,
    }
    .to_body(0)
    .unwrap()
}

#[tokio::test]
async fn test_custom_rail_proof_credited_once_per_settlement() {
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        supplies_id: true,
        fee: None,
    });
    let mut wallet = joined(
        &w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        &rail,
    )
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = joined(
        &w,
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        &rail,
    )
    .await;
    let version = list_version(1, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.0015")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The payee credits it, by the settlement its rail reads from the ledger's proof.
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    // Another proof of that settlement, a replay, names the same record: still one.
    let proof = rail.last.lock().unwrap().clone().unwrap();
    let shop_id = shop.app_id();
    let inject = |proof: Map<String, Value>| {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_id],
            "paykit.payment_proof",
            Vec::new(),
            proof_body(shop_id, version, proof),
        );
    };
    inject(proof);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    // A proof of another settlement is another payment: a record of its own.
    inject(
        json!({ "type": "x-ledger", "entry": "another" })
            .as_object()
            .cloned()
            .unwrap(),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 2);
    // A proof whose spelling the rail reads no settlement from isn't credited.
    inject(
        json!({ "type": "x-other", "entry": "third" })
            .as_object()
            .cloned()
            .unwrap(),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 2);
}

/// Alice's wallet and bob's shop on `rail`, the wallet holding the shop's list, and the
/// outcome of paying the list's reusable endpoint.
async fn pay_through(w: &World, rail: &Arc<LedgerRail>) -> paykit_sdk::Result<PayOutcome> {
    pay_through_with(w, rail, PaykitConfig::default()).await
}

/// [`pay_through`], the wallet with `config`.
async fn pay_through_with(
    w: &World,
    rail: &Arc<LedgerRail>,
    config: PaykitConfig,
) -> paykit_sdk::Result<PayOutcome> {
    let mut wallet = joined_with(
        w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        rail,
        config,
    )
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = joined(
        w,
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        rail,
    )
    .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.0015")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .outcome
}

#[tokio::test]
async fn test_custom_rail_any_fee_paid_with_no_cap() {
    // With no fee cap, the default, Paykit makes no check of the fee (Paykit spec 7.2 step
    // 1): a method that charges nothing pays, and so does one that may charge something.
    for fee in ["0", "0.00000001"] {
        let w = World::new();
        let rail = Arc::new(LedgerRail {
            fake: Arc::clone(&w.rail),
            last: Mutex::new(None),
            supplies_id: true,
            fee: Some(paykit_lib::Decimal::parse(fee).unwrap()),
        });
        let outcome = pay_through(&w, &rail).await.unwrap();
        assert!(
            matches!(outcome, PayOutcome::Paid { .. }),
            "{fee}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn test_custom_rail_fee_cap_is_per_asset() {
    // A fee of 0.00000010 BTC: paid under a BTC cap at least that, refused under one below
    // it, and paid under a cap of another asset only, which leaves BTC uncapped.
    let fee = || Some(paykit_lib::Decimal::parse("0.0000001").unwrap());
    let btc = || paykit_lib::Asset::new("btc").unwrap();
    let cap = |value: &str| paykit_lib::Decimal::parse(value).unwrap();
    for (config, pays) in [
        (
            PaykitConfig::default().with_fee_cap(btc(), cap("0.0000001")),
            true,
        ),
        (
            PaykitConfig::default().with_fee_cap(btc(), cap("0.00000009")),
            false,
        ),
        (
            PaykitConfig::default().with_fee_cap(paykit_lib::Asset::new("usd").unwrap(), cap("1")),
            true,
        ),
    ] {
        let w = World::new();
        let rail = Arc::new(LedgerRail {
            fake: Arc::clone(&w.rail),
            last: Mutex::new(None),
            supplies_id: true,
            fee: fee(),
        });
        let outcome = pay_through_with(&w, &rail, config).await;
        if pays {
            assert!(
                matches!(outcome, Ok(PayOutcome::Paid { .. })),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(
                    outcome.as_ref().map_err(|e| e.refusal()),
                    Err(Some(paykit_sdk::Refusal::Transfer { .. }))
                ),
                "{outcome:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_payee_deletion_a_restored_custom_receipt_waits_for_its_payment_method() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        supplies_id: true,
        fee: None,
    });
    let mut wallet = joined(
        &w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        &rail,
    )
    .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = joined(
        &w,
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        &rail,
    )
    .await;
    let version = list_version(1, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.0015")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The payee credits it, by the settlement its rail reads from the ledger's proof.
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);

    let key = shop.payee_activity().await.unwrap()[0].record;
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    let proof = rail.last.lock().unwrap().clone().unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::payee_payment_record(
            &endpoint("btc-lightning-bolt12"),
            &rail.settlement_id(&proof).unwrap(),
        ),
    );
    let mut seller_wallet = joined(
        &w,
        &w.bob,
        "seller-wallet",
        AppScopes::new().request().build().unwrap(),
        &rail,
    )
    .await;
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let mut unavailable = Paykit::restore(
        w.env(&w.bob, store()),
        Default::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(unavailable
        .retained_payee_receipts()
        .await
        .unwrap()
        .is_empty());
    let kept = unavailable.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        Default::default(),
        &kept,
        rail,
        w.timer(),
    )
    .await
    .unwrap();
    let receipts = restored.retained_payee_receipts().await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].record, key);
    assert_eq!(receipts[0].receipt.message().msg_id, receipt);
}

#[tokio::test]
async fn test_custom_rail_requested_proof_without_a_settlement_id_keeps_its_own_warning() {
    use paykit_sdk::{NoticeKind, ReceiptDetails, Refusal, SettlementSubject};
    use pubky_switch_client::app::UserConfirmed;
    use pubky_switch_client::clock::Clock;
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: w.rail.clone(),
        last: Mutex::new(None),
        supplies_id: false,
        fee: None,
    });
    let mut buyer = joined(
        &w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().manage_contacts().build().unwrap(),
        &rail,
    )
    .await;
    let mut shop = joined(
        &w,
        &w.bob,
        "shop",
        AppScopes::new()
            .request()
            .manage_contacts()
            .build()
            .unwrap(),
        &rail,
    )
    .await;
    buyer.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut prior = None;
    for _ in 0..2 {
        let sent = shop
            .send_request(w.alice_id(), crate::common::draft("1", w.clock.now_ms()))
            .await
            .unwrap()
            .request
            .unwrap();
        buyer.receive(None).await.unwrap();
        assert!(matches!(
            Box::pin(buyer.pay_request(&sent, PayOptions::default()))
                .await
                .outcome
                .unwrap(),
            PayOutcome::Paid { .. }
        ));
        shop.receive(None).await.unwrap();
        let record = shop
            .payee_activity()
            .await
            .unwrap()
            .into_iter()
            .find(|v| v.request_hash == Some(sent.request_hash))
            .unwrap()
            .record;
        for _ in 0..2 {
            assert_eq!(
                shop.confirm_payment(&record).await.unwrap_err().refusal(),
                Some(&Refusal::MaybeCredited)
            );
        }
        let warnings = shop.notices().await.unwrap();
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert_eq!(warning.kind, NoticeKind::CreditNeedsUser);
        assert!(
            matches!(warning.settlement,Some(SettlementSubject::RequestedProof {record: key,..}) if key == record)
        );
        assert_ne!(prior.as_ref(), Some(&warning.id));
        shop.acknowledge_notice(&warning.id).await.unwrap();
        prior = Some(warning.id.clone());
        let backup = shop.backup().await.unwrap();
        drop(shop);
        shop = Paykit::restore(
            w.env(&w.bob, store()),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        shop.receive(None).await.unwrap();
        assert!(shop.notices().await.unwrap().is_empty());
        let id = shop
            .confirm_payment_with(
                &record,
                ReceiptDetails::new().with_maybe_credited_confirmed(UserConfirmed::new()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shop.confirm_payment(&record).await.unwrap(), Some(id));
    }
    assert_eq!(w.rail.all_executions().values().sum::<u64>(), 2);
}
