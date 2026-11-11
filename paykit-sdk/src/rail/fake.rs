//! A fake payment rail, for tests only (feature `test-support`).
//!
//! It pays a payload in memory, keyed by the idempotency key, and can be told to fail, hang
//! or err for the next payment. It is idempotent on the key unless told otherwise, and keeps
//! a history every device sees. Its proofs are `{ "type": "bitcoin-txid", "txid": ... }`,
//! whose settlement ID Paykit wire 1.5 fixes, so a payee credits a payment without a request
//! by it. A payload `...?amount=<value>` fixes the amount it transfers, as an invoice does.
//!
//! It pays the identifiers of [`FAKE_ENDPOINT_IDENTIFIERS`], in that order, unless told
//! another order or another choice, and keeps every set of candidates it was handed.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::transport::BoxFuture;
use serde_json::{Map, Value};

use super::{
    CandidateId, EndpointSelection, PaymentEndpointCandidate, ProofCheck, Rail, RailDeclaration,
    RailError, RailOutcome, RailPaid, RailPayment, Transfer,
};

/// A choice of candidates in place of the fake rail's own.
type Choice = Arc<
    dyn Fn(
            &[PaymentEndpointCandidate],
            Option<&PaymentAmount>,
        ) -> Result<EndpointSelection, RailError>
        + Send
        + Sync,
>;

/// The longest a fake dispatch takes to complete or fail, as its declaration says: an hour.
pub const FAKE_LONGEST_COMPLETION_MS: u64 = 3_600_000;

/// What the next new payment does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Behaviour {
    /// Paid at once.
    Succeed,
    /// Fails at once: no money moved.
    Fail,
    /// Stays in flight until [`FakeRail::settle`].
    Hang,
    /// The call errs while the payment goes through: the payer gets no answer.
    LoseAnswer,
    /// The method refuses the call: nothing is submitted ([`RailError::Refused`]).
    Refuse,
}

#[derive(Clone, Debug)]
struct Paid {
    endpoint: String,
    amount: PaymentAmount,
    outcome: RailOutcome,
}

#[derive(Default)]
struct Inner {
    payments: BTreeMap<AttemptId, Paid>,
    executions: BTreeMap<AttemptId, u64>,
    next: VecDeque<Behaviour>,
    counter: u64,
    idempotent: bool,
    forgotten: Vec<AttemptId>,
    calls: BTreeMap<AttemptId, u64>,
    declaration: Option<RailDeclaration>,
    endpoints: Option<Vec<PaymentEndpointIdentifier>>,
    choice: Option<Choice>,
    max_fee: Option<paykit_lib::Decimal>,
    next_fees: VecDeque<Option<paykit_lib::Decimal>>,
    selections: Vec<(Vec<PaymentEndpointCandidate>, Option<PaymentAmount>)>,
}

/// An in-memory payment rail shared by payers and payees. Tests only.
pub struct FakeRail {
    inner: Mutex<Inner>,
    clock: Option<Arc<dyn Clock>>,
}

impl std::fmt::Debug for FakeRail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FakeRail({} payments)", self.lock().payments.len())
    }
}

impl Default for FakeRail {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeRail {
    /// A rail where every payment succeeds unless told otherwise.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                idempotent: true,
                ..Inner::default()
            }),
            clock: None,
        }
    }

    /// The same, stamping each payment's verified payment time from `clock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock: Some(clock),
            ..Self::new()
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn now(&self) -> Option<u64> {
        self.clock.as_ref().map(|c| c.now_ms())
    }

    /// Queue what the next new payment does.
    pub fn next(&self, behaviour: Behaviour) {
        self.lock().next.push_back(behaviour);
    }

    /// Make the rail idempotent on the key, or not: then a repeat pays again.
    pub fn set_idempotent(&self, idempotent: bool) {
        self.lock().idempotent = idempotent;
    }

    /// Pay `endpoints`, in that order, from now on, instead of the default
    /// ([`FAKE_ENDPOINT_IDENTIFIERS`]): of the candidates handed in, those with one of these
    /// identifiers, by identifier in this order, then in the order handed in.
    pub fn set_endpoint_identifiers(&self, endpoints: Vec<PaymentEndpointIdentifier>) {
        self.lock().endpoints = Some(endpoints);
    }

    /// Answer [`Rail::select_endpoints`] with `choose` from now on, instead of by its
    /// identifiers, whatever it returns.
    pub fn set_selection(
        &self,
        choose: impl Fn(
                &[PaymentEndpointCandidate],
                Option<&PaymentAmount>,
            ) -> Result<EndpointSelection, RailError>
            + Send
            + Sync
            + 'static,
    ) {
        self.lock().choice = Some(Arc::new(choose));
    }

    /// Report `max_fee` as the largest fee of every transfer from now on, instead of none.
    pub fn set_max_fee(&self, max_fee: Option<paykit_lib::Decimal>) {
        self.lock().max_fee = max_fee;
    }

    /// Report `fees` as the largest fee of the next transfers, one each, in order, then the
    /// one [`FakeRail::set_max_fee`] set: a fee estimate that changes between calls.
    pub fn next_max_fees(&self, fees: Vec<Option<paykit_lib::Decimal>>) {
        self.lock().next_fees.extend(fees);
    }

    /// Every set of candidates [`Rail::select_endpoints`] was handed, with its amount, in
    /// the order of the calls.
    pub fn selections(&self) -> Vec<(Vec<PaymentEndpointCandidate>, Option<PaymentAmount>)> {
        self.lock().selections.clone()
    }

    /// The identifiers it pays, in its order.
    fn identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        if let Some(endpoints) = &self.lock().endpoints {
            return endpoints.clone();
        }
        FAKE_ENDPOINT_IDENTIFIERS
            .iter()
            .filter_map(|id| PaymentEndpointIdentifier::new(id).ok())
            .collect()
    }

    /// Declare `declaration` from now on instead of the default (idempotent while
    /// [`FakeRail::set_idempotent`] says so, a history kept for good that survives a
    /// restore). A declaration without a history makes [`Rail::payment`] show none.
    pub fn set_declaration(&self, declaration: RailDeclaration) {
        let mut inner = self.lock();
        inner.idempotent = declaration.idempotent;
        inner.declaration = Some(declaration);
    }

    /// How many times [`Rail::pay`] was called under `key`, repeats included.
    pub fn pay_calls(&self, key: &AttemptId) -> u64 {
        self.lock().calls.get(key).copied().unwrap_or(0)
    }

    /// How many times money moved under `key`: more than one is a second execution.
    pub fn executions(&self, key: &AttemptId) -> u64 {
        self.lock().executions.get(key).copied().unwrap_or(0)
    }

    /// Every key money moved under, with its count.
    pub fn all_executions(&self) -> BTreeMap<AttemptId, u64> {
        self.lock().executions.clone()
    }

    /// The payer's history loses the record of `key`, as a restored device's local wallet
    /// history would. The payment itself stands.
    pub fn forget_history(&self, key: &AttemptId) {
        self.lock().forgotten.push(*key);
    }

    /// Finish a payment left in flight: paid or failed.
    pub fn settle(&self, key: &AttemptId, paid: bool) {
        let now = self.now();
        let mut inner = self.lock();
        let counter = inner.counter;
        if let Some(payment) = inner.payments.get_mut(key) {
            if payment.outcome == RailOutcome::Pending {
                payment.outcome = if paid {
                    paid_outcome(key, counter, now)
                } else {
                    RailOutcome::Failed
                };
            }
        }
    }

    /// A payment the method reported failed completes after all: its history shows it paid,
    /// and money moved once.
    pub fn complete_after_failure(&self, key: &AttemptId) {
        let now = self.now();
        let mut inner = self.lock();
        let counter = inner.counter;
        let completed = inner.payments.get_mut(key).is_some_and(|payment| {
            if payment.outcome != RailOutcome::Failed {
                return false;
            }
            payment.outcome = paid_outcome(key, counter, now);
            true
        });
        if completed {
            let count = inner.executions.entry(*key).or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    /// What a payload fixes: `...?amount=<value>`.
    fn fixed_amount(payload: &str, amount: &PaymentAmount) -> Option<Option<PaymentAmount>> {
        let (_, query) = payload.split_once('?')?;
        let value = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == "amount")
            .map(|(_, v)| v)?;
        if value == "unknown" {
            return Some(None);
        }
        Some(
            paykit_lib::Decimal::new(value)
                .ok()
                .map(|value| PaymentAmount {
                    value,
                    asset: amount.asset.clone(),
                }),
        )
    }
}

fn paid_outcome(key: &AttemptId, counter: u64, now: Option<u64>) -> RailOutcome {
    let digest =
        pubky_switch_core::enc::hash(&[key.to_bytes().as_slice(), &counter.to_be_bytes()].concat());
    let txid: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let mut proof = Map::new();
    proof.insert("type".into(), Value::String("bitcoin-txid".into()));
    proof.insert("txid".into(), Value::String(txid));
    let paid = RailPaid::new(proof);
    RailOutcome::Paid(match now {
        Some(ms) => paid.with_paid_at_ms(ms),
        None => paid,
    })
}

/// The Payment Endpoint Identifiers the fake rail pays by default, in the order it tries
/// them.
pub const FAKE_ENDPOINT_IDENTIFIERS: [&str; 4] = [
    "btc-bitcoin-p2tr",
    "btc-bitcoin-p2wpkh",
    "btc-lightning-bolt11",
    "btc-lightning-bolt12",
];

impl Rail for FakeRail {
    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        amount: Option<&PaymentAmount>,
    ) -> Result<EndpointSelection, RailError> {
        let choice = {
            let mut inner = self.lock();
            inner
                .selections
                .push((candidates.to_vec(), amount.cloned()));
            inner.choice.clone()
        };
        if let Some(choose) = choice {
            return choose(candidates, amount);
        }
        let payable: Vec<CandidateId> = self
            .identifiers()
            .iter()
            .flat_map(|id| {
                candidates
                    .iter()
                    .filter(move |candidate| &candidate.endpoint_id == id)
                    .map(|candidate| candidate.id)
            })
            .collect();
        Ok(EndpointSelection::new(payable))
    }

    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        self.identifiers()
    }

    fn declaration(&self) -> RailDeclaration {
        let inner = self.lock();
        if let Some(declared) = inner.declaration {
            return declared;
        }
        RailDeclaration::new(FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(inner.idempotent)
            .with_history(true)
            .with_history_survives_restore(true)
    }

    fn transfer(
        &self,
        _endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        let fee = {
            let mut inner = self.lock();
            let max_fee = inner.max_fee.clone();
            inner.next_fees.pop_front().unwrap_or(max_fee)
        };
        Transfer::new(
            Self::fixed_amount(payload.as_str(), amount).unwrap_or(Some(amount.clone())),
            fee,
        )
    }

    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        let now = self.now();
        let answer = {
            let mut inner = self.lock();
            let key = payment.idempotency_key;
            let calls = inner.calls.entry(key).or_insert(0);
            *calls = calls.saturating_add(1);
            let repeat = inner.payments.get(&key).cloned();
            match repeat {
                Some(held) if inner.idempotent => Ok(held.outcome),
                _ => {
                    let behaviour = inner.next.pop_front().unwrap_or(Behaviour::Succeed);
                    if behaviour == Behaviour::Refuse {
                        return Box::pin(async move {
                            Err(RailError::Refused("the method refused the payment".into()))
                        });
                    }
                    inner.counter = inner.counter.saturating_add(1);
                    let counter = inner.counter;
                    let outcome = match behaviour {
                        Behaviour::Succeed | Behaviour::LoseAnswer => {
                            paid_outcome(&key, counter, now)
                        }
                        // A refusal was answered above.
                        Behaviour::Fail | Behaviour::Refuse => RailOutcome::Failed,
                        Behaviour::Hang => RailOutcome::Pending,
                    };
                    if outcome != RailOutcome::Failed {
                        let count = inner.executions.entry(key).or_insert(0);
                        *count = count.saturating_add(1);
                    }
                    let amount = Self::fixed_amount(payment.payload.as_str(), &payment.amount)
                        .flatten()
                        .unwrap_or(payment.amount.clone());
                    inner.payments.insert(
                        key,
                        Paid {
                            endpoint: payment.payload.as_str().to_owned(),
                            amount,
                            outcome: outcome.clone(),
                        },
                    );
                    if behaviour == Behaviour::LoseAnswer {
                        Err(RailError::Unavailable("the answer was lost".into()))
                    } else {
                        Ok(outcome)
                    }
                }
            }
        };
        Box::pin(async move { answer })
    }

    fn payment(
        &self,
        idempotency_key: AttemptId,
    ) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>> {
        let inner = self.lock();
        let keeps_history = inner.declaration.is_none_or(|d| d.history);
        let answer = if !keeps_history || inner.forgotten.contains(&idempotency_key) {
            None
        } else {
            inner
                .payments
                .get(&idempotency_key)
                .map(|p| p.outcome.clone())
        };
        drop(inner);
        Box::pin(async move { Ok(answer) })
    }

    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        let inner = self.lock();
        let answer = inner.payments.get(&check.attempt_id).and_then(|paid| {
            let RailOutcome::Paid(RailPaid {
                proof, paid_at_ms, ..
            }) = &paid.outcome
            else {
                return None;
            };
            let payload_ok = check
                .payload
                .as_ref()
                .is_none_or(|p| p.as_str() == paid.endpoint);
            (payload_ok && *proof == check.proof && paid.amount == check.amount)
                .then(|| paid_at_ms.unwrap_or(0))
        });
        drop(inner);
        Box::pin(async move { Ok(answer) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(value: &str) -> PaymentAmount {
        PaymentAmount {
            value: paykit_lib::Decimal::new(value).unwrap(),
            asset: paykit_lib::Asset::new("btc").unwrap(),
        }
    }

    fn payment(key: u8, payload: &str) -> RailPayment {
        RailPayment {
            endpoint_id: PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            payload: PaymentEndpointPayload::new(payload).unwrap(),
            amount: amount("1"),
            idempotency_key: AttemptId::from_bytes([key; 16]),
            payment_reference: None,
        }
    }

    #[tokio::test]
    async fn test_fake_rail_is_idempotent_on_the_key() {
        let rail = FakeRail::new();
        let first = rail.pay(payment(1, "lnbc1")).await.unwrap();
        let again = rail.pay(payment(1, "lnbc1")).await.unwrap();
        assert_eq!(first, again);
        assert_eq!(rail.executions(&AttemptId::from_bytes([1; 16])), 1);
    }

    #[tokio::test]
    async fn test_fake_rail_not_idempotent_pays_again() {
        let rail = FakeRail::new();
        rail.set_idempotent(false);
        rail.pay(payment(2, "lnbc1")).await.unwrap();
        rail.pay(payment(2, "lnbc1")).await.unwrap();
        assert_eq!(rail.executions(&AttemptId::from_bytes([2; 16])), 2);
    }

    #[tokio::test]
    async fn test_fake_rail_failure_moves_nothing_and_hang_settles() {
        let rail = FakeRail::new();
        rail.next(Behaviour::Fail);
        rail.next(Behaviour::Hang);
        assert_eq!(
            rail.pay(payment(3, "a")).await.unwrap(),
            RailOutcome::Failed
        );
        assert_eq!(rail.executions(&AttemptId::from_bytes([3; 16])), 0);
        assert_eq!(
            rail.pay(payment(4, "a")).await.unwrap(),
            RailOutcome::Pending
        );
        rail.settle(&AttemptId::from_bytes([4; 16]), true);
        let history = rail.payment(AttemptId::from_bytes([4; 16])).await.unwrap();
        assert!(matches!(history, Some(RailOutcome::Paid(_))));
    }

    #[tokio::test]
    async fn test_fake_rail_lost_answer_pays_and_history_shows_it() {
        let rail = FakeRail::new();
        rail.next(Behaviour::LoseAnswer);
        assert!(rail.pay(payment(5, "a")).await.is_err());
        let key = AttemptId::from_bytes([5; 16]);
        assert!(matches!(
            rail.payment(key).await.unwrap(),
            Some(RailOutcome::Paid(_))
        ));
        rail.forget_history(&key);
        assert_eq!(rail.payment(key).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_fake_rail_verifies_only_the_proof_it_made() {
        let rail = FakeRail::new();
        let RailOutcome::Paid(RailPaid { proof, .. }) = rail.pay(payment(6, "a")).await.unwrap()
        else {
            panic!("paid");
        };
        assert!(paykit_lib::derive::settlement_id(&proof).is_some());
        let check = |proof: Map<String, Value>, value: &str| ProofCheck {
            endpoint_id: PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            payload: None,
            amount: amount(value),
            attempt_id: AttemptId::from_bytes([6; 16]),
            proof,
        };
        assert!(rail
            .verify(check(proof.clone(), "1"))
            .await
            .unwrap()
            .is_some());
        assert!(rail.verify(check(proof, "2")).await.unwrap().is_none());
        assert!(rail.verify(check(Map::new(), "1")).await.unwrap().is_none());
    }

    #[test]
    fn test_fake_rail_selection_follows_its_identifiers_then_the_order_handed() {
        let rail = FakeRail::new();
        let candidate = |id: u32, endpoint: &str| {
            PaymentEndpointCandidate::new(
                CandidateId::new(id),
                PaymentEndpointIdentifier::new(endpoint).unwrap(),
                PaymentEndpointPayload::new("payload").unwrap(),
            )
        };
        let candidates = [
            candidate(0, "btc-lightning-bolt11"),
            candidate(1, "btc-bitcoin-p2wpkh"),
            candidate(2, "x-unpaid"),
            candidate(3, "btc-lightning-bolt11"),
        ];
        let ids = |answer: Result<EndpointSelection, RailError>| {
            let answer = answer.unwrap();
            assert!(answer.errors.is_empty());
            answer
                .payable
                .into_iter()
                .map(CandidateId::get)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(rail.select_endpoints(&candidates, None)), [1, 0, 3]);
        rail.set_endpoint_identifiers(vec![PaymentEndpointIdentifier::new("x-unpaid").unwrap()]);
        assert_eq!(ids(rail.select_endpoints(&candidates, None)), [2]);
        rail.set_selection(|candidates, _| Ok(EndpointSelection::new(vec![candidates[3].id])));
        assert_eq!(
            ids(rail.select_endpoints(&candidates, Some(&amount("1")))),
            [3]
        );

        let handed = rail.selections();
        assert_eq!(handed.len(), 3);
        assert_eq!(handed[2], (candidates.to_vec(), Some(amount("1"))));
    }

    #[test]
    fn test_fake_rail_transfer_reads_a_fixed_amount() {
        let rail = FakeRail::new();
        let id = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let fixed = PaymentEndpointPayload::new("lnbc?amount=2").unwrap();
        let open = PaymentEndpointPayload::new("bc1q").unwrap();
        let unknown = PaymentEndpointPayload::new("lnbc?amount=unknown").unwrap();
        assert_eq!(
            rail.transfer(&id, &fixed, &amount("1")).amount,
            Some(amount("2"))
        );
        assert_eq!(
            rail.transfer(&id, &open, &amount("1")).amount,
            Some(amount("1"))
        );
        assert_eq!(rail.transfer(&id, &unknown, &amount("1")).amount, None);
    }
}
