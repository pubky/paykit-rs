//! The bindings' runtime: time limits on host calls, the refused method and re-entrant calls.
use super::storage::DeviceStore;
use crate::*;
use paykit_lib::AttemptId;
use paykit_sdk::{Rail, RailError};
use pubky_switch_client::{app::HomeserverReader, transport::TransportError};
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

fn stalled_host(root: &std::path::Path, history: bool) -> Result<Arc<WalletHost>, WalletError> {
    WalletHost::new(
        Arc::new(DeviceStore::open(root)),
        Arc::new(Stalled { history }),
        Arc::new(Stalled { history }),
        Vec::new(),
    )
}

/// Poll `call` once, as a host thread would when it starts a wallet call.
pub(super) fn first_poll<T>(call: impl Future<Output = T>) -> Poll<T> {
    let mut call = std::pin::pin!(call);
    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_lookup_past_its_limit_is_not_now() {
    let rail = HostRail::new(Arc::new(Stalled { history: true }), Vec::new()).unwrap();
    let attempt = AttemptId::from_bytes([3; 16]);
    assert!(matches!(
        rail.payment(attempt).await,
        Err(RailError::Unavailable(_))
    ));
    assert!(matches!(
        rail.allowance_history([4; 32]).await,
        Err(RailError::Unavailable(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn test_host_calls_homeserver_past_its_limit_times_out() {
    let root = tempfile::tempdir().unwrap();
    let host = stalled_host(root.path(), true).unwrap();
    assert!(matches!(
        host.home.get([5; 32], "/pub/example".into()).await,
        Err(TransportError::Timeout)
    ));
}

#[test]
fn test_host_calls_refuse_a_method_without_history() {
    let root = tempfile::tempdir().unwrap();
    assert!(matches!(
        stalled_host(root.path(), false),
        Err(WalletError::InvalidInput { field }) if field == "payment method history"
    ));
    assert!(stalled_host(root.path(), true).is_ok());
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
