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
use pubky_dispatch_client::testing::TestSigner;
use pubky_dispatch_client::transport::BoxFuture;
use serde_json::{json, Map, Value};

use crate::common::{btc, endpoint, list_version, store, World};

/// The fake rail, as a ledger whose proofs read `{ "type": "x-ledger", "entry": ... }`
/// rather than the fake rail's own spelling, and which names the settlements of its own
/// endpoints only ([`LEDGER_ENDPOINTS`]). An entry with an `output` names that output's settlement apart
/// from the others of the entry.
struct LedgerRail {
    fake: Arc<FakeRail>,
    last: Mutex<Option<Map<String, Value>>>,
    supplies_id: bool,
    /// Whether its proofs read `{ "type": "bitcoin-txid", "txid": "L-<entry>" }`: the
    /// spelling of another method, and not a valid one there.
    lookalike: bool,
    /// The output each payment's proof names in its entry, by its attempt, if outputs are
    /// named: a new attempt takes the next one, and the same attempt always the same.
    outputs: Mutex<Option<std::collections::BTreeMap<AttemptId, u64>>>,
}

/// The list's endpoint paid without a request, whose settlements the ledger names.
const LEDGER_ENDPOINT: &str = "btc-lightning-bolt12";

/// The endpoints whose settlements the ledger names: its own, and none of the others.
const LEDGER_ENDPOINTS: [&str; 2] = ["btc-lightning-bolt11", LEDGER_ENDPOINT];

impl LedgerRail {
    fn to_ledger(&self, attempt: AttemptId, proof: &Map<String, Value>) -> Map<String, Value> {
        let tx = proof["tx"].as_str().unwrap();
        let mut value = if self.lookalike {
            json!({ "type": "bitcoin-txid", "txid": format!("L-{tx}") })
        } else {
            json!({ "type": "x-ledger", "entry": tx })
        };
        if let Some(outputs) = self.outputs.lock().unwrap().as_mut() {
            let next = outputs.len() as u64;
            value["output"] = json!(*outputs.entry(attempt).or_insert(next));
        }
        value.as_object().cloned().unwrap()
    }

    /// The ledger's entry, and output if any, of one of its proofs.
    fn entry(&self, proof: &Map<String, Value>) -> Option<String> {
        if self.lookalike {
            (proof.get("type")?.as_str()? == "bitcoin-txid")
                .then(|| {
                    proof
                        .get("txid")?
                        .as_str()?
                        .strip_prefix("L-")
                        .map(str::to_owned)
                })
                .flatten()
        } else {
            (proof.get("type")?.as_str()? == "x-ledger")
                .then(|| proof.get("entry")?.as_str().map(str::to_owned))
                .flatten()
        }
    }

    fn to_fake(&self, proof: &Map<String, Value>) -> Map<String, Value> {
        json!({ "rail": "fake", "tx": self.entry(proof) })
            .as_object()
            .cloned()
            .unwrap()
    }
}

impl LedgerRail {
    fn outcome(&self, attempt: AttemptId, outcome: RailOutcome) -> RailOutcome {
        match outcome {
            RailOutcome::Paid(mut paid) => {
                paid.proof = self.to_ledger(attempt, &paid.proof);
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
        Transfer::new(self.fake.transfer(endpoint_id, payload, amount).amount)
    }

    fn fraction_digits(&self, endpoint_id: &PaymentEndpointIdentifier) -> Option<u32> {
        self.fake.fraction_digits(endpoint_id)
    }

    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        let attempt = payment.idempotency_key;
        Box::pin(async move {
            self.fake
                .pay(payment)
                .await
                .map(|o| self.outcome(attempt, o))
        })
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
                .map(|o| self.outcome(idempotency_key, o)))
        })
    }

    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        let mut check = check;
        check.proof = self.to_fake(&check.proof);
        Box::pin(async move { self.fake.verify(check).await })
    }

    /// The ledger's entry names its settlement, with its output when it has one, at the
    /// ledger's endpoints; elsewhere it names none.
    fn settlement_id(
        &self,
        endpoint_id: &paykit_lib::PaymentEndpointIdentifier,
        proof: &Map<String, Value>,
    ) -> Option<Vec<u8>> {
        if !self.supplies_id || !LEDGER_ENDPOINTS.contains(&endpoint_id.as_str()) {
            return None;
        }
        let entry = self.entry(proof)?;
        Some(match proof.get("output").and_then(Value::as_u64) {
            Some(output) => format!("{entry}:{output}").into_bytes(),
            None => entry.into_bytes(),
        })
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

/// A proof alice's wallet sends bob's shop for its list's endpoint `at`, `proof`.
fn proof_body_at(
    shop: [u8; 16],
    version: paykit_lib::ListVersion,
    at: &str,
    proof: Map<String, Value>,
) -> Map<String, Value> {
    proof_body_of(shop, version, at, AttemptId::generate().unwrap(), proof)
}

/// [`proof_body_at`], of the payment `attempt`.
fn proof_body_of(
    shop: [u8; 16],
    version: paykit_lib::ListVersion,
    at: &str,
    attempt: AttemptId,
    proof: Map<String, Value>,
) -> Map<String, Value> {
    PaymentProof {
        attempt_id: attempt,
        billing_period: None,
        payment_endpoint_identifier: endpoint(at),
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
        lookalike: false,
        outputs: Mutex::new(None),
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
    let inject_at = |at: &str, proof: Value| {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_id],
            "paykit.payment_proof",
            Vec::new(),
            proof_body_at(shop_id, version, at, proof.as_object().cloned().unwrap()),
        );
    };
    let inject = |proof: Map<String, Value>| inject_at(LEDGER_ENDPOINT, Value::Object(proof));
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
    // A proof whose spelling the rail reads no settlement from isn't credited, whatever
    // method it looks like: Paykit reads no proof itself (Paykit wire 1.5).
    for other in [
        json!({ "type": "x-other", "entry": "third" }),
        json!({
            "type": "bitcoin-txid",
            "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b"
        }),
        json!({ "rail": "fake", "tx": "t1" }),
    ] {
        inject(other.as_object().cloned().unwrap());
        let read = shop.receive(None).await.unwrap();
        assert!(dropped_unnamed(&read.report.events), "{other}: {read:?}");
        assert_eq!(shop.payee_activity().await.unwrap().len(), 2, "{other}");
    }
    // The method's answer is for the endpoint paid as well as the proof: the same proof at
    // an endpoint the ledger names nothing for isn't credited, and at its own one is.
    let same = json!({ "type": "x-ledger", "entry": "fifth" });
    inject_at("btc-bitcoin-p2wpkh", same.clone());
    let read = shop.receive(None).await.unwrap();
    assert!(dropped_unnamed(&read.report.events), "{read:?}");
    assert_eq!(shop.payee_activity().await.unwrap().len(), 2);
    inject_at(LEDGER_ENDPOINT, same);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 3);
}

#[tokio::test]
async fn test_custom_rail_two_payments_the_method_names_apart_are_both_credited() {
    use paykit_sdk::rail::Behaviour;
    use paykit_sdk::PayeeStatus;
    // Two payments in one ledger entry, its outputs 0 and 1, which the method names as two
    // settlements: the payee credits both, and a replay of either credits nothing more.
    // Whether one transaction carries several payments is the method's (Paykit wire 1.5).
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        supplies_id: true,
        lookalike: false,
        outputs: Mutex::new(Some(std::collections::BTreeMap::new())),
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
    let mut attempts = Vec::new();
    for _ in 0..2 {
        w.rail.next(Behaviour::InSettlement([9; 32]));
        let outcome = wallet
            .pay_without_request(
                UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
                    .with_options(PayOptions::default().with_endpoint(endpoint(LEDGER_ENDPOINT))),
            )
            .await
            .outcome
            .unwrap();
        let PayOutcome::Paid { attempt_id, .. } = outcome else {
            panic!("{outcome:?}")
        };
        attempts.push(attempt_id);
    }
    let replay = rail.last.lock().unwrap().clone().unwrap();
    assert_eq!(replay["output"], 1);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 2);
    let mut receipts = std::collections::BTreeMap::new();
    for sale in &sales {
        let receipt = shop.confirm_payment(&sale.record).await.unwrap().unwrap();
        receipts.insert(sale.record, receipt);
    }
    let receipted = |sales: &[paykit_sdk::PayeeActivity]| {
        sales
            .iter()
            .filter(|sale| sale.status == PayeeStatus::Receipted)
            .count()
    };
    assert_eq!(receipted(&shop.payee_activity().await.unwrap()), 2);
    // The second payment's proof again, of its own attempt under a new message ID, is that
    // settlement again: no third record, and its receipt stays the one given.
    let shop_id = shop.app_id();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop_id],
        "paykit.payment_proof",
        Vec::new(),
        proof_body_of(shop_id, version, LEDGER_ENDPOINT, attempts[1], replay),
    );
    shop.receive(None).await.unwrap();
    let after = shop.payee_activity().await.unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(receipted(&after), 2);
    for sale in &after {
        assert_eq!(
            shop.confirm_payment(&sale.record).await.unwrap().as_ref(),
            receipts.get(&sale.record)
        );
    }
}

/// Whether the read dropped a proof because its payment method names no settlement for it.
fn dropped_unnamed(events: &[pubky_dispatch_client::app::AppEvent]) -> bool {
    events.iter().any(|event| {
        matches!(event, pubky_dispatch_client::app::AppEvent::MessageDropped { reason, .. }
            if reason.contains("names no settlement"))
    })
}

#[tokio::test]
async fn test_custom_rail_unrequested_proof_without_a_rail_is_dropped_with_its_reason() {
    use pubky_dispatch_client::app::{App, ProtocolSet};
    // A protocol with no rail names no settlement: an unrequested proof is dropped with
    // that reason, never applied, and no record is written (Paykit wire 1.5).
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        supplies_id: true,
        lookalike: false,
        outputs: Mutex::new(None),
    });
    let _wallet = joined(
        &w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        &rail,
    )
    .await;
    let protocol = Arc::new(paykit_sdk::PaykitProtocol::new());
    let mut shop = App::join(
        w.env(&w.bob, store()),
        ProtocolSet::new(vec![protocol]).unwrap(),
        PaykitConfig::default().app,
        w.approve(&w.bob, "shop", &AppScopes::new().request().build().unwrap()),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    let shop_id = shop.app_id();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop_id],
        "paykit.payment_proof",
        Vec::new(),
        proof_body_at(
            shop_id,
            list_version(1, 1),
            LEDGER_ENDPOINT,
            json!({ "type": "x-ledger", "entry": "first" })
                .as_object()
                .cloned()
                .unwrap(),
        ),
    );
    let read = shop.receive(None).await.unwrap();
    assert!(dropped_unnamed(&read.events), "{read:?}");
    assert!(shop
        .records_of(paykit_lib::collections::Collection::PayeeActivity.name())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_custom_rail_requested_proof_spelled_like_another_method_credited_by_the_rail() {
    // The ledger's proofs here look like another method's, and aren't valid in that
    // method's spelling. The ledger verifies them and names their settlement, so the payee
    // credits the request: the rail's answer is the answer (Paykit wire 1.5).
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: w.rail.clone(),
        last: Mutex::new(None),
        supplies_id: true,
        lookalike: true,
        outputs: Mutex::new(None),
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
    let sent = shop
        .send_request(
            w.alice_id(),
            crate::common::draft("1", pubky_dispatch_client::clock::Clock::now_ms(&*w.clock)),
        )
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
    let proof = rail.last.lock().unwrap().clone().unwrap();
    assert_eq!(proof["type"], "bitcoin-txid");
    shop.receive(None).await.unwrap();
    let record = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|v| v.request_hash == Some(sent.request_hash))
        .unwrap()
        .record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

#[test]
fn test_custom_rail_paykit_reads_no_proof_itself() {
    // Paykit defines no settlement ID and knows no payment method's proof: the rail the
    // host supplies reads its own proofs (Paykit wire 1.5). Only the test rail, behind the
    // test-only feature, has a proof spelling of its own.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for dir in ["paykit-lib/src", "paykit-sdk/src"] {
        let mut stack = vec![root.join(dir)];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("paykit-sdk/src/rail/fake.rs")
                {
                    files.push(path);
                }
            }
        }
    }
    assert!(files.len() > 20, "{files:?}");
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for needle in [
            "bitcoin-txid",
            "\"txid\"",
            "proof.get(",
            ".proof.get(",
            "derive::settlement_id",
        ] {
            assert!(!text.contains(needle), "{}: {needle}", path.display());
        }
    }
}

#[tokio::test]
async fn test_payee_deletion_a_restored_custom_receipt_waits_for_its_payment_method() {
    use paykit_lib::collections::Collection;
    use pubky_dispatch_client::app::{Collection as DispatchCollection, RecordTarget};
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        supplies_id: true,
        lookalike: false,
        outputs: Mutex::new(None),
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
        DispatchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::payee_payment_record(
            &endpoint("btc-lightning-bolt12"),
            &rail
                .settlement_id(&endpoint("btc-lightning-bolt12"), &proof)
                .unwrap(),
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
    use pubky_dispatch_client::app::UserConfirmed;
    use pubky_dispatch_client::clock::Clock;
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: w.rail.clone(),
        last: Mutex::new(None),
        supplies_id: false,
        lookalike: false,
        outputs: Mutex::new(None),
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
