//! A rail of a method `paykit-lib` doesn't know (Paykit wire 1.5): its proofs name their
//! settlement in a spelling of its own, which the rail reads ([`Rail::settlement_id`]), so
//! a payee credits a payment without a request through it, once per settlement.

use std::sync::{Arc, Mutex};

use paykit_lib::bodies::PaymentProof;
use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_sdk::rail::{
    FakeRail, ProofCheck, RailDeclaration, RailError, RailOutcome, RailPayment, Transfer,
};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, RequestedScopes,
    UnrequestedPayment,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::TestSigner;
use pubky_switch_client::transport::BoxFuture;
use serde_json::{json, Map, Value};

use crate::common::{btc, endpoint, store, World};

/// The fake rail, as a ledger whose proofs read `{ "type": "x-ledger", "entry": ... }`
/// rather than the `bitcoin-txid` spelling `paykit-lib` knows.
struct LedgerRail {
    fake: Arc<FakeRail>,
    last: Mutex<Option<Map<String, Value>>>,
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
            RailOutcome::Paid { proof, paid_at_ms } => {
                let proof = to_ledger(&proof);
                *self.last.lock().unwrap() = Some(proof.clone());
                RailOutcome::Paid { proof, paid_at_ms }
            }
            other => other,
        }
    }
}

impl Rail for LedgerRail {
    fn declaration(&self) -> RailDeclaration {
        self.fake.declaration()
    }

    fn transfer(
        &self,
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        Transfer {
            max_fee: self.fee.clone(),
            ..self.fake.transfer(endpoint_id, payload, amount)
        }
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
        let check = ProofCheck {
            proof: to_fake(&check.proof),
            ..check
        };
        Box::pin(async move { self.fake.verify(check).await })
    }

    /// The ledger's entry names its settlement.
    fn settlement_id(&self, proof: &Map<String, Value>) -> Option<Vec<u8>> {
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
    let mut app = Paykit::join(
        w.env(signer, store()),
        PaykitConfig::default(),
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
fn proof_body(shop: [u8; 16], version: u64, proof: Map<String, Value>) -> Map<String, Value> {
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
    let version = w.clock.now_ms();
    w.list_to_alice(&mut shop, &[wallet.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(UnrequestedPayment {
            payee: w.bob_id(),
            amount: btc("0.0015"),
            options: PayOptions {
                endpoint: Some(endpoint("btc-lightning-bolt12")),
                ..PayOptions::default()
            },
        })
        .await
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
    let mut wallet = joined(
        w,
        &w.alice,
        "wallet",
        AppScopes::new().pay().build().unwrap(),
        rail,
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_without_request(UnrequestedPayment {
            payee: w.bob_id(),
            amount: btc("0.0015"),
            options: PayOptions {
                endpoint: Some(endpoint("btc-lightning-bolt12")),
                ..PayOptions::default()
            },
        })
        .await
}

#[tokio::test]
async fn test_custom_rail_zero_fee_needs_no_cap() {
    // A method that charges nothing pays under the default config, which sets no fee cap.
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        fee: Some(paykit_lib::Decimal::zero()),
    });
    let outcome = pay_through(&w, &rail).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // One that may charge something is refused without a cap.
    let w = World::new();
    let rail = Arc::new(LedgerRail {
        fake: Arc::clone(&w.rail),
        last: Mutex::new(None),
        fee: Some(paykit_lib::Decimal::parse("0.00000001").unwrap()),
    });
    let refused = pay_through(&w, &rail).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::Transfer(_))),
        "{refused:?}"
    );
}
