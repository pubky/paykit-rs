//! The app record's upkeep at a receive is held to its own outcome (Paykit spec 4): none of
//! the receive's other steps depends on the record, so a failure that is only the
//! publisher's leaves the record owed, the receive's other work done and the host told in the
//! summary; a wallet-wide stop still ends the receive at once.

use paykit_sdk::{Paykit, RequestRef, RequestStatus};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;

use super::oversized_capabilities;
use crate::common::{draft, endpoint, refuse_writes, request_from, World};
use crate::stored::kept;

/// How the upkeep of the record fails at the receive under test.
#[derive(Clone, Copy)]
enum UpkeepFailure {
    /// The payment method pays a list the record can't carry: the Switch client refuses the
    /// update before anything is signed (Switch wire 6.1).
    Uncarriable,
    /// The switch refuses the record's delivery for good.
    SwitchRefuses,
    /// The own switch can't be reached for the record's delivery.
    Unreachable,
    /// The switch answers the record's delivery that the app's certificate expired: a
    /// wallet-wide stop (Switch spec 3.6).
    Stop,
}

/// Alice's wallet with bob's request stored unrecorded, its record's write refused, and
/// bob's cancellation of it sent and not yet read. The upkeep of the wallet's record is due
/// at the next receive, and fails as `failure` says. Returns the wallet and the request.
async fn upkeep_due_over_pending_work(w: &World, failure: UpkeepFailure) -> (Paykit, RequestRef) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The receive step's write is refused for good, and so is the app's first write of it.
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    // The payment method pays another list now: the record's upkeep writes at the next
    // receive, its first write after the read, before the request's own.
    let fault = |code| Fault::Error {
        path: "/ops".into(),
        code,
        current: serde_json::Value::Null,
    };
    match failure {
        UpkeepFailure::Uncarriable => w.rail.set_endpoint_identifiers(oversized_capabilities()),
        UpkeepFailure::SwitchRefuses => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice
                .switch()
                .fail_next(fault(ErrorCode::PermissionMissing));
        }
        UpkeepFailure::Unreachable => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice.switch().fail_next(Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            });
        }
        UpkeepFailure::Stop => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice.switch().fail_next(fault(ErrorCode::CertExpired));
        }
    }
    (wallet, request)
}

/// The upkeep fails short of a wallet-wide stop: the stored request is written and bob's
/// cancellation placed over it in the same receive, and the summary tells the host the
/// record is owed. A later receive brings the record up to date.
async fn work_done_past_failed_upkeep(failure: UpkeepFailure) {
    let w = World::new();
    let (mut wallet, request) = upkeep_due_over_pending_work(&w, failure).await;
    let signed = wallet.app().record().clone();
    let app_id = wallet.app_id();
    let at_switch = || w.alice.switch().app(&app_id).record.unwrap();
    let delivered = at_switch();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.app_record_owed.is_some(), "{summary:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    // Placed in the request record, not only kept: the kept copy is gone.
    assert!(kept(&wallet, "kc/").await.is_empty());
    // Nothing new reached the switch.
    assert_eq!(at_switch(), delivered);
    if let UpkeepFailure::Uncarriable = failure {
        // Refused before anything is signed: the record is the one signed before.
        assert_eq!(wallet.app().record(), &signed);
        w.rail
            .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
    }
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.app_record_owed, None, "{summary:?}");
    // The record brought up to date is the one the switch holds now.
    assert_ne!(at_switch(), delivered);
    assert_eq!(&at_switch(), wallet.app().record());
}

#[tokio::test]
async fn test_own_record_uncarriable_capabilities_leave_the_receive_work_done() {
    work_done_past_failed_upkeep(UpkeepFailure::Uncarriable).await;
}

#[tokio::test]
async fn test_own_record_refused_delivery_leaves_the_receive_work_done() {
    work_done_past_failed_upkeep(UpkeepFailure::SwitchRefuses).await;
}

#[tokio::test]
async fn test_own_record_unreachable_delivery_leaves_the_receive_work_done() {
    work_done_past_failed_upkeep(UpkeepFailure::Unreachable).await;
}

#[tokio::test]
async fn test_own_record_upkeep_stop_ends_the_receive() {
    let w = World::new();
    let (mut wallet, request) = upkeep_due_over_pending_work(&w, UpkeepFailure::Stop).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::CertExpired), "{error:?}");
    // Nothing after the upkeep ran: the request is still unrecorded and bob's cancellation
    // only kept.
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    assert!(!kept(&wallet, "kc/").await.is_empty());
}
