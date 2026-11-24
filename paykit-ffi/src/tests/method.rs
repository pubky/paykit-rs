use crate::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

pub(super) struct Account {
    file: PathBuf,
    /// The clock the account stamps each payment's verified time from.
    clock: Arc<dyn pubky_switch_client::clock::Clock>,
    pub calls: AtomicUsize,
    pub lookups: AtomicUsize,
    /// A proof check never answers.
    pub stall_verify: AtomicBool,
    /// After its charge, `pay` throws instead of answering.
    pub throw_after_charge: AtomicBool,
    /// `transfer` and `settlement_id` throw instead of answering.
    pub throw_callbacks: AtomicBool,
    /// A payment lookup answers that the payment hasn't settled yet.
    pub lookup_pending: AtomicBool,
    pub park: AtomicBool,
    pub entered: Notify,
    pub finish: Notify,
    /// Raw JSON members every proof carries after its `payment`, as written, such as
    /// `,"fee":1.50`.
    pub proof_members: std::sync::Mutex<String>,
    coverage: std::sync::Mutex<Coverage>,
}
#[derive(Clone, Copy)]
enum Coverage {
    Complete,
    Incomplete,
    Unavailable,
}
impl Account {
    pub fn open(root: &Path, clock: Arc<dyn pubky_switch_client::clock::Clock>) -> Self {
        let file = root.join("account.sqlite");
        Connection::open(&file).unwrap().execute_batch("CREATE TABLE IF NOT EXISTS payments(attempt TEXT PRIMARY KEY,asset TEXT NOT NULL,amount TEXT NOT NULL,endpoint TEXT NOT NULL,payload TEXT NOT NULL,paid_at INTEGER NOT NULL);").unwrap();
        Self {
            file,
            clock,
            calls: AtomicUsize::new(0),
            lookups: AtomicUsize::new(0),
            stall_verify: AtomicBool::new(false),
            throw_after_charge: AtomicBool::new(false),
            throw_callbacks: AtomicBool::new(false),
            lookup_pending: AtomicBool::new(false),
            park: AtomicBool::new(true),
            entered: Notify::new(),
            finish: Notify::new(),
            proof_members: std::sync::Mutex::new(String::new()),
            coverage: std::sync::Mutex::new(Coverage::Complete),
        }
    }
    pub fn count(&self) -> usize {
        Connection::open(&self.file)
            .unwrap()
            .query_row("SELECT count(*) FROM payments", [], |r| r.get(0))
            .unwrap()
    }
    /// What the host declares for this account; `history: false` for a method that can't look
    /// a payment up.
    pub fn declaration(history: bool) -> MethodDeclaration {
        MethodDeclaration {
            idempotent: true,
            idempotency_retention_ms: None,
            history,
            history_retention_ms: None,
            history_survives_restore: true,
            longest_completion_ms: 1000,
            payment_time_tolerance_ms: 7_200_000,
        }
    }
    /// The endpoint the account pays, with its fractional digits.
    pub fn endpoints() -> Vec<(paykit_lib::PaymentEndpointIdentifier, Option<u32>)> {
        vec![(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            Some(8),
        )]
    }
    /// A rail over this account, as the host would declare it.
    pub fn rail(self: &Arc<Self>) -> HostRail {
        HostRail::new(self.clone(), Self::declaration(true), Self::endpoints())
    }
    /// A rail over this account declared without a history.
    pub fn rail_without_history(self: &Arc<Self>) -> HostRail {
        HostRail::new(self.clone(), Self::declaration(false), Self::endpoints())
    }
    pub fn paid(&self, id: &str) -> MethodOutcome {
        let members = self.proof_members.lock().unwrap().clone();
        MethodOutcome::Paid {
            proof_json: format!(r#"{{"payment":"{id}"{members}}}"#).into_bytes(),
            paid_at_ms: None,
        }
    }
}
#[async_trait::async_trait]
impl PaymentMethod for Account {
    fn select_endpoints(
        &self,
        candidates: Vec<EndpointCandidate>,
        _: Option<Amount>,
    ) -> Result<SelectedEndpoints, MethodError> {
        Ok(SelectedEndpoints {
            payable: candidates
                .into_iter()
                .filter(|c| c.identifier == "btc-lightning-bolt11")
                .map(|c| c.id)
                .collect(),
            failures: Vec::new(),
        })
    }
    fn transfer(
        &self,
        _: String,
        _: String,
        amount: Amount,
    ) -> Result<Arc<MethodTransfer>, MethodError> {
        if self.throw_callbacks.load(Ordering::SeqCst) {
            return Err(MethodError::Unknown {
                reason: "transfer threw".into(),
            });
        }
        Ok(MethodTransfer::new(Some(amount)).unwrap())
    }
    async fn pay(&self, p: MethodPayment) -> Result<MethodOutcome, MethodError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Connection::open(&self.file)
            .unwrap()
            .execute(
                "INSERT INTO payments VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(attempt) DO NOTHING",
                params![
                    p.attempt_id,
                    p.amount.asset,
                    p.amount.value,
                    p.endpoint,
                    p.payload,
                    i64::try_from(self.clock.now_ms()).unwrap()
                ],
            )
            .unwrap();
        self.entered.notify_one();
        if self.park.load(Ordering::SeqCst) {
            self.finish.notified().await;
        }
        if self.throw_after_charge.load(Ordering::SeqCst) {
            return Err(MethodError::Unknown {
                reason: "answer lost after the charge".into(),
            });
        }
        Ok(self.paid(&p.attempt_id))
    }
    async fn payment(&self, id: String) -> Result<Option<MethodOutcome>, MethodError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if self.lookup_pending.load(Ordering::SeqCst) {
            return Ok(Some(MethodOutcome::Pending));
        }
        let paid = Connection::open(&self.file)
            .unwrap()
            .query_row("SELECT attempt FROM payments WHERE attempt=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .unwrap();
        Ok(paid.as_deref().map(|id| self.paid(id)))
    }
    async fn allowance_history(
        &self,
        _: Vec<u8>,
    ) -> Result<Option<Vec<MethodDispatch>>, MethodError> {
        match *self.coverage.lock().unwrap() {
            Coverage::Complete => {}
            Coverage::Incomplete => return Ok(None),
            Coverage::Unavailable => {
                return Err(MethodError::Unknown {
                    reason: "account history unavailable".into(),
                })
            }
        }
        let db = Connection::open(&self.file).unwrap();
        let mut query = db
            .prepare("SELECT attempt,asset,amount FROM payments ORDER BY attempt")
            .unwrap();
        let rows = query
            .query_map([], |r| {
                let id: String = r.get(0)?;
                Ok(MethodDispatch {
                    outcome: self.paid(&id),
                    attempt_id: id,
                    amount: Amount {
                        asset: r.get(1)?,
                        value: r.get(2)?,
                    },
                })
            })
            .unwrap();
        Ok(Some(rows.collect::<Result<_, _>>().unwrap()))
    }
    async fn verify(&self, p: MethodProof) -> Result<Option<u64>, MethodError> {
        if self.stall_verify.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let proof: serde_json::Value = serde_json::from_slice(&p.proof_json).unwrap();
        if proof["payment"].as_str() != Some(p.attempt_id.as_str()) {
            return Ok(None);
        }
        // The proof the payee holds carries every member the account wrote, by value.
        let members = self.proof_members.lock().unwrap().clone();
        let written: serde_json::Value =
            serde_json::from_str(&format!(r#"{{"payment":"{}"{members}}}"#, p.attempt_id)).unwrap();
        if written
            .as_object()
            .unwrap()
            .iter()
            .any(|(k, v)| proof.get(k) != Some(v))
        {
            return Ok(None);
        }
        let found=Connection::open(&self.file).unwrap().query_row("SELECT paid_at FROM payments WHERE attempt=?1 AND asset=?2 AND amount=?3 AND endpoint=?4 AND (?5 IS NULL OR payload=?5)",params![p.attempt_id,p.amount.asset,p.amount.value,p.endpoint,p.payload],|r|r.get::<_,i64>(0)).optional().unwrap();
        Ok(found.map(|at| u64::try_from(at).unwrap()))
    }
    /// The account names a settlement by the endpoint paid and the proof's `payment`, so
    /// two endpoints whose proofs carry the same value are two settlements.
    fn settlement_id(
        &self,
        endpoint: String,
        proof: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, MethodError> {
        if self.throw_callbacks.load(Ordering::SeqCst) {
            return Err(MethodError::Unknown {
                reason: "settlement id threw".into(),
            });
        }
        Ok(serde_json::from_slice::<serde_json::Value>(&proof)
            .ok()
            .and_then(|proof| Some(proof.get("payment")?.as_str()?.to_owned()))
            .map(|payment| format!("{endpoint}/{payment}").into_bytes()))
    }
}

#[test]
fn test_method_settlement_id_gets_the_endpoint_through_the_host_callback() {
    use paykit_sdk::Rail;
    let directory = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(
        directory.path(),
        Arc::new(pubky_switch_client::clock::TestClock::new(0)),
    ));
    let rail = account.rail();
    let proof = serde_json::json!({ "payment": "same" })
        .as_object()
        .cloned()
        .unwrap();
    let first = paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let second = paykit_lib::PaymentEndpointIdentifier::new("btc-bitcoin-p2wpkh").unwrap();
    // The same inner value at two endpoints is two settlements, as the method names them.
    assert_eq!(
        rail.settlement_id(&first, &proof),
        Some(b"btc-lightning-bolt11/same".to_vec())
    );
    assert_eq!(
        rail.settlement_id(&second, &proof),
        Some(b"btc-bitcoin-p2wpkh/same".to_vec())
    );
    // A proof the method names no settlement for stays unnamed.
    let none = serde_json::json!({ "other": "same" })
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(rail.settlement_id(&first, &none), None);
}

#[tokio::test]
async fn test_method_history_distinguishes_complete_empty_from_missing_and_unavailable() {
    use paykit_sdk::{Rail, RailError};
    let directory = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(
        directory.path(),
        Arc::new(pubky_switch_client::clock::TestClock::new(0)),
    ));
    let rail = account.rail();
    let payer = [17; 32];
    assert_eq!(
        rail.allowance_history(payer).await.unwrap(),
        Some(Vec::new())
    );
    *account.coverage.lock().unwrap() = Coverage::Incomplete;
    assert_eq!(rail.allowance_history(payer).await.unwrap(), None);
    *account.coverage.lock().unwrap() = Coverage::Unavailable;
    assert!(matches!(
        rail.allowance_history(payer).await,
        Err(RailError::Unavailable(_))
    ));
}

#[test]
fn test_method_declaration_forwards_the_payment_time_tolerance() {
    use paykit_sdk::Rail;
    let directory = tempfile::tempdir().unwrap();
    let rail = Arc::new(Account::open(
        directory.path(),
        Arc::new(pubky_switch_client::clock::TestClock::new(0)),
    ))
    .rail();
    assert_eq!(rail.declaration().payment_time_tolerance_ms, 7_200_000);
    assert_eq!(rail.declaration().longest_completion_ms, 1000);
}

#[test]
fn test_method_thrown_transfer_and_settlement_id_answer_unknown_and_none() {
    use paykit_lib::{Asset, Decimal, PaymentAmount, PaymentEndpointPayload};
    use paykit_sdk::Rail;
    let directory = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(
        directory.path(),
        Arc::new(pubky_switch_client::clock::TestClock::new(0)),
    ));
    let rail = account.rail();
    let (endpoint, _) = Account::endpoints().remove(0);
    let payload = PaymentEndpointPayload::new("lnbc1").unwrap();
    let amount = PaymentAmount {
        asset: Asset::new("btc").unwrap(),
        value: Decimal::new("7").unwrap(),
    };
    let proof = serde_json::Map::from_iter([("payment".into(), serde_json::json!("a1"))]);
    assert_eq!(
        rail.transfer(&endpoint, &payload, &amount).amount,
        Some(amount.clone())
    );
    assert_eq!(
        rail.settlement_id(&endpoint, &proof),
        Some(format!("{}/a1", endpoint.as_str()).into_bytes())
    );
    assert_eq!(rail.fraction_digits(&endpoint), Some(8));
    account.throw_callbacks.store(true, Ordering::SeqCst);
    // A thrown transfer is an amount the method can't tell, which the user confirms.
    let transfer = rail.transfer(&endpoint, &payload, &amount);
    assert_eq!(transfer.amount, None);
    assert_eq!(rail.settlement_id(&endpoint, &proof), None);
}

#[test]
fn test_method_fraction_digits_come_from_the_declared_endpoints() {
    use paykit_sdk::Rail;
    let directory = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(
        directory.path(),
        Arc::new(pubky_switch_client::clock::TestClock::new(0)),
    ));
    let other = paykit_lib::PaymentEndpointIdentifier::new("btc-onchain").unwrap();
    let rail = HostRail::new(
        account,
        Account::declaration(true),
        vec![(other.clone(), None)],
    );
    assert_eq!(rail.fraction_digits(&other), None);
    let (lightning, _) = Account::endpoints().remove(0);
    assert_eq!(rail.fraction_digits(&lightning), None);
}

#[test]
fn test_method_paid_proof_settles_whatever_numbers_it_holds() {
    let paid = |proof: &[u8]| {
        crate::MethodOutcome::Paid {
            proof_json: proof.to_vec(),
            paid_at_ms: None,
        }
        .into_rail()
    };
    // The money moved: a proof that reads settles the payment and is carried, whatever its
    // numbers, so a resume never meets a refusal for it.
    for proof in [
        br#"{"settlement":"a1","fee":1.50}"#.as_slice(),
        br#"{"settlement":18446744073709551617}"#,
    ] {
        assert!(matches!(paid(proof), Ok(paykit_sdk::RailOutcome::Paid(_))));
    }
    // A repeated member is no refusal either: decoding keeps the last value, as it always has.
    let Ok(paykit_sdk::RailOutcome::Paid(repeated)) = paid(br#"{"tx":"a","tx":"b"}"#) else {
        panic!("a proof with a repeated member settles the payment");
    };
    assert_eq!(repeated.proof.get("tx"), Some(&serde_json::json!("b")));
    assert!(matches!(
        paid(b"not json"),
        Err(paykit_sdk::RailError::Unavailable(_))
    ));
}
