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
    pub calls: AtomicUsize,
    pub park: AtomicBool,
    pub entered: Notify,
    pub finish: Notify,
    coverage: std::sync::Mutex<Coverage>,
}
#[derive(Clone, Copy)]
enum Coverage {
    Complete,
    Incomplete,
    Unavailable,
}
impl Account {
    pub fn open(root: &Path) -> Self {
        let file = root.join("account.sqlite");
        Connection::open(&file).unwrap().execute_batch("CREATE TABLE IF NOT EXISTS payments(attempt TEXT PRIMARY KEY,asset TEXT NOT NULL,amount TEXT NOT NULL,endpoint TEXT NOT NULL,payload TEXT NOT NULL);").unwrap();
        Self {
            file,
            calls: AtomicUsize::new(0),
            park: AtomicBool::new(true),
            entered: Notify::new(),
            finish: Notify::new(),
            coverage: std::sync::Mutex::new(Coverage::Complete),
        }
    }
    pub fn count(&self) -> usize {
        Connection::open(&self.file)
            .unwrap()
            .query_row("SELECT count(*) FROM payments", [], |r| r.get(0))
            .unwrap()
    }
    pub fn paid(id: &str) -> MethodOutcome {
        MethodOutcome::Paid {
            proof_json: serde_json::to_vec(&serde_json::json!({"payment":id})).unwrap(),
            paid_at_ms: None,
        }
    }
}
#[async_trait::async_trait]
impl PaymentMethod for Account {
    fn declaration(&self) -> MethodDeclaration {
        MethodDeclaration {
            idempotent: true,
            idempotency_retention_ms: None,
            history: true,
            history_retention_ms: None,
            history_survives_restore: true,
            longest_completion_ms: 1000,
        }
    }
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
    fn transfer(&self, _: String, _: String, amount: Amount) -> Arc<MethodTransfer> {
        MethodTransfer::new(Some(amount), None).unwrap()
    }
    fn fraction_digits(&self, _: String) -> Option<u32> {
        Some(8)
    }
    async fn pay(&self, p: MethodPayment) -> Result<MethodOutcome, MethodError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Connection::open(&self.file)
            .unwrap()
            .execute(
                "INSERT INTO payments VALUES(?1,?2,?3,?4,?5) ON CONFLICT(attempt) DO NOTHING",
                params![
                    p.attempt_id,
                    p.amount.asset,
                    p.amount.value,
                    p.endpoint,
                    p.payload
                ],
            )
            .unwrap();
        self.entered.notify_one();
        if self.park.load(Ordering::SeqCst) {
            self.finish.notified().await;
        }
        Ok(Self::paid(&p.attempt_id))
    }
    async fn payment(&self, id: String) -> Result<Option<MethodOutcome>, MethodError> {
        let paid = Connection::open(&self.file)
            .unwrap()
            .query_row("SELECT attempt FROM payments WHERE attempt=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .unwrap();
        Ok(paid.as_deref().map(Self::paid))
    }
    async fn allowance_history(
        &self,
        _: Vec<u8>,
    ) -> Result<Option<Vec<MethodDispatch>>, MethodError> {
        match *self.coverage.lock().unwrap() {
            Coverage::Complete => {}
            Coverage::Incomplete => return Ok(None),
            Coverage::Unavailable => {
                return Err(MethodError::Unavailable {
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
                    outcome: Self::paid(&id),
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
        let proof: serde_json::Value = serde_json::from_slice(&p.proof_json).unwrap();
        if proof["payment"].as_str() != Some(p.attempt_id.as_str()) {
            return Ok(None);
        }
        let found=Connection::open(&self.file).unwrap().query_row("SELECT 1 FROM payments WHERE attempt=?1 AND asset=?2 AND amount=?3 AND endpoint=?4 AND (?5 IS NULL OR payload=?5)",params![p.attempt_id,p.amount.asset,p.amount.value,p.endpoint,p.payload],|r|r.get::<_,u8>(0)).optional().unwrap();
        Ok(found.map(|_| 1))
    }
    fn settlement_id(&self, proof: Vec<u8>) -> Option<Vec<u8>> {
        serde_json::from_slice::<serde_json::Value>(&proof)
            .ok()?
            .get("payment")?
            .as_str()
            .map(|s| s.as_bytes().to_vec())
    }
}

#[tokio::test]
async fn test_method_history_distinguishes_complete_empty_from_missing_and_unavailable() {
    use paykit_sdk::{Rail, RailError};
    let directory = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(directory.path()));
    let rail = HostRail {
        method: account.clone(),
        identifiers: Vec::new(),
    };
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
