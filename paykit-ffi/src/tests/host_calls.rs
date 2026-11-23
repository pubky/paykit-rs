//! The bindings' runtime: time limits on host calls, the refused method and re-entrant calls.
use super::storage::DeviceStore;
use crate::*;
use paykit_lib::AttemptId;
use paykit_sdk::{Rail, RailError};
use pubky_switch_client::transport::TransportError;
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Waker},
};

/// A host whose asynchronous calls never answer.
struct Stalled {
    history: bool,
}

#[async_trait::async_trait]
impl PaymentMethod for Stalled {
    fn declaration(&self) -> MethodDeclaration {
        MethodDeclaration {
            idempotent: true,
            idempotency_retention_ms: None,
            history: self.history,
            history_retention_ms: None,
            history_survives_restore: true,
            longest_completion_ms: 1000,
            payment_time_tolerance_ms: 0,
        }
    }
    fn select_endpoints(
        &self,
        _: Vec<EndpointCandidate>,
        _: Option<Amount>,
    ) -> Result<SelectedEndpoints, MethodError> {
        Ok(SelectedEndpoints {
            payable: Vec::new(),
            failures: Vec::new(),
        })
    }
    fn transfer(&self, _: String, _: String, amount: Amount) -> Arc<MethodTransfer> {
        MethodTransfer::new(Some(amount), None).unwrap()
    }
    fn fraction_digits(&self, _: String) -> Option<u32> {
        None
    }
    async fn pay(&self, _: MethodPayment) -> Result<MethodOutcome, MethodError> {
        std::future::pending().await
    }
    async fn payment(&self, _: String) -> Result<Option<MethodOutcome>, MethodError> {
        std::future::pending().await
    }
    async fn allowance_history(
        &self,
        _: Vec<u8>,
    ) -> Result<Option<Vec<MethodDispatch>>, MethodError> {
        std::future::pending().await
    }
    async fn verify(&self, _: MethodProof) -> Result<Option<u64>, MethodError> {
        std::future::pending().await
    }
    fn settlement_id(&self, _: Vec<u8>) -> Option<Vec<u8>> {
        None
    }
}

#[async_trait::async_trait]
impl WalletHomeserver for Stalled {
    async fn get(&self, _: Vec<u8>, _: String) -> Result<Option<Vec<u8>>, HomeserverError> {
        std::future::pending().await
    }
    async fn put(&self, _: Vec<u8>, _: String, _: Vec<u8>) -> Result<(), HomeserverError> {
        std::future::pending().await
    }
    async fn delete(&self, _: Vec<u8>, _: String) -> Result<(), HomeserverError> {
        std::future::pending().await
    }
}

fn stalled_host(root: &std::path::Path, history: bool) -> Arc<WalletHost> {
    WalletHost::new(
        Arc::new(DeviceStore::open(root)),
        Arc::new(Stalled { history }),
        Arc::new(Stalled { history }),
        Vec::new(),
    )
    .unwrap()
}

/// Poll `call` once, as a host thread would when it starts a wallet call.
pub(super) fn first_poll<T>(call: impl Future<Output = T>) -> Poll<T> {
    let mut call = std::pin::pin!(call);
    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// Await `call`, failing the test instead of hanging if its own limit never answers.
async fn within<T>(call: impl Future<Output = T>) -> T {
    tokio::time::timeout(runtime::HOST_CALL_LIMIT * 3, call)
        .await
        .expect("the call answered within its limit")
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_lookup_past_its_limit_is_not_now() {
    let rail = HostRail::new(Arc::new(Stalled { history: true }), Vec::new());
    let attempt = AttemptId::from_bytes([3; 16]);
    assert!(matches!(
        within(rail.payment(attempt)).await,
        Err(RailError::Unavailable(_))
    ));
    assert!(matches!(
        within(rail.allowance_history([4; 32])).await,
        Err(RailError::Unavailable(_))
    ));
    // `verify` waits through the same bound; its proof check is the SDK's own type.
    let stalled = Stalled { history: true };
    let proof = MethodProof {
        endpoint: "btc-lightning-bolt11".into(),
        payload: None,
        amount: Amount {
            asset: "btc".into(),
            value: "7".into(),
        },
        attempt_id: attempt.to_text(),
        proof_json: b"{}".to_vec(),
    };
    assert!(matches!(
        within(crate::method::bounded(stalled.verify(proof))).await,
        Err(RailError::Unavailable(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_homeserver_past_its_limit_times_out() {
    let root = tempfile::tempdir().unwrap();
    let host = stalled_host(root.path(), true);
    assert!(matches!(
        within(host.home.get([5; 32], "/pub/example".into())).await,
        Err(TransportError::Timeout)
    ));
    // A write past its limit has an unknown outcome, as for any transport timeout.
    assert!(matches!(
        within(host.writer.put([5; 32], "/pub/example".into(), vec![1])).await,
        Err(TransportError::Timeout)
    ));
    assert!(matches!(
        within(host.writer.delete([5; 32], "/pub/example".into())).await,
        Err(TransportError::Timeout)
    ));
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_running_pay_stops_counting_as_in_flight() {
    use crate::flights::{Flights, Waited, STALE_AFTER};
    let flights = Flights::default();
    let limit = std::time::Duration::from_secs(1);
    let attempt = "running-attempt".to_string();
    let never = std::future::pending::<Result<MethodOutcome, MethodError>>();
    let waited = within(flights.dispatch(attempt.clone(), None, never, limit)).await;
    assert!(matches!(waited, Waited::StillRunning));
    assert!(flights.in_flight(&attempt));
    tokio::time::sleep(limit * STALE_AFTER).await;
    // Past its bound the host's lookup decides, as the method contract makes it authoritative.
    assert!(!flights.in_flight(&attempt));
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_payments_in_flight_are_shared_by_every_host() {
    let first = HostRail::new(Arc::new(Stalled { history: true }), Vec::new());
    let second = HostRail::new(Arc::new(Stalled { history: true }), Vec::new());
    let attempt = AttemptId::from_bytes([6; 16]).to_text();
    let never = std::future::pending::<Result<MethodOutcome, MethodError>>();
    let limit = std::time::Duration::from_secs(1);
    within(
        first
            .flights()
            .dispatch(attempt.clone(), None, never, limit),
    )
    .await;
    // Another host in the same process sees the attempt in flight and never asks its method.
    assert!(second.flights().in_flight(&attempt));
    assert!(matches!(
        within(second.payment(AttemptId::parse(&attempt).unwrap())).await,
        Ok(Some(paykit_sdk::RailOutcome::Pending))
    ));
}

#[test]
fn test_host_calls_open_a_wallet_with_a_method_without_history() {
    let root = tempfile::tempdir().unwrap();
    // Opening, reading, receiving and backing up work with any declaration; paying is
    // refused (the wallet journey covers both).
    let host = stalled_host(root.path(), false);
    assert!(!host.rail.has_lookup());
    assert!(stalled_host(root.path(), true).rail.has_lookup());
}

#[test]
fn test_reentrant_call_from_a_host_callback_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let enrollment = WalletEnrollment::new(Arc::new(DeviceStore::open(root.path())));
    // From inside a synchronous callback the call is refused before it is sent.
    let refused = runtime::in_callback(|| first_poll(enrollment.begin_enrollment(vec![1; 16])));
    assert!(matches!(refused, Poll::Ready(Err(WalletError::Reentrant))));
    // The same call from an ordinary host thread starts and waits for its answer.
    assert!(first_poll(enrollment.begin_enrollment(vec![1; 16])).is_pending());
}
