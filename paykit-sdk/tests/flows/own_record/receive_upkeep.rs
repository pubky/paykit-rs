//! The app record's upkeep at a receive is held to its own outcome (Paykit spec 4): none of
//! the receive's other steps depends on the record, so a failure every later step would take
//! as one to try again later leaves the record owed, the receive's other work done and the
//! host told in the summary; any other failure still ends the receive at once.

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
    /// The own switch answers the record's delivery that it is unavailable now.
    SwitchUnavailable,
    /// The own switch can't be reached for the record's delivery.
    Unreachable,
    /// The switch answers the record's delivery with `code`, which a later write would take
    /// as a refusal, or as a wallet-wide stop, rather than one to try again later.
    Refused(ErrorCode),
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
        UpkeepFailure::SwitchUnavailable => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice.switch().fail_next(fault(ErrorCode::Unavailable));
        }
        UpkeepFailure::Unreachable => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice.switch().fail_next(Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            });
        }
        UpkeepFailure::Refused(code) => {
            w.rail
                .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
            w.alice.switch().fail_next(fault(code));
        }
    }
    (wallet, request)
}

/// The upkeep fails as a later step would try again later: the stored request is written and bob's
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
async fn test_own_record_unavailable_switch_leaves_the_receive_work_done() {
    work_done_past_failed_upkeep(UpkeepFailure::SwitchUnavailable).await;
}

#[tokio::test]
async fn test_own_record_unreachable_delivery_leaves_the_receive_work_done() {
    work_done_past_failed_upkeep(UpkeepFailure::Unreachable).await;
}

/// The upkeep's delivery is answered with `code`: the receive ends with that answer, as
/// before, and nothing after the upkeep ran.
async fn receive_ended_by_the_upkeep(code: ErrorCode) {
    let w = World::new();
    let (mut wallet, request) =
        upkeep_due_over_pending_work(&w, UpkeepFailure::Refused(code)).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert_eq!(error.code(), Some(code), "{error:?}");
    // The request is still unrecorded and bob's cancellation only kept.
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    assert!(!kept(&wallet, "kc/").await.is_empty());
}

#[tokio::test]
async fn test_own_record_upkeep_stop_ends_the_receive() {
    // A wallet-wide stop (Switch spec 3.6).
    receive_ended_by_the_upkeep(ErrorCode::CertExpired).await;
}

#[tokio::test]
async fn test_own_record_upkeep_refused_by_the_switch_ends_the_receive() {
    // A refusal the stored entries' writes would count toward their window, not try again.
    receive_ended_by_the_upkeep(ErrorCode::PermissionMissing).await;
}

#[tokio::test]
async fn test_own_record_homeserver_write_owed_told_at_the_receive() {
    // The switch takes the record listing what the payment method pays now, and its
    // Homeserver write fails: the receive tells the host the record is owed, and the next one,
    // which writes it, clears that.
    let w = World::new();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .request()
                .pay()
                .build()
                .unwrap(),
            super::publishing(),
            crate::common::store(),
        )
        .await;
    w.rail
        .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
    w.homeserver.fail_writes(1, false);
    let summary = shop.receive(None).await.unwrap();
    let owed = summary.app_record_owed.unwrap();
    assert!(owed.contains("Homeserver"), "{owed}");
    let summary = shop.receive(None).await.unwrap();
    assert_eq!(summary.app_record_owed, None, "{summary:?}");
}

#[tokio::test]
async fn test_own_record_unchanged_record_with_its_homeserver_write_owed_told() {
    // The record goes to the client unchanged at the upkeep, and its Homeserver write, owed
    // since the last one failed, fails again: the receive tells the host it is owed.
    let w = World::new();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .request()
                .pay()
                .build()
                .unwrap(),
            super::publishing(),
            crate::common::store(),
        )
        .await;
    w.rail
        .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
    w.homeserver.fail_writes(1, false);
    let signed = shop.receive(None).await.unwrap();
    assert!(signed.app_record_owed.is_some(), "{signed:?}");
    let held = shop.app().record().clone();
    // Nothing to change now; every Homeserver write of this receive fails, the client's own
    // at its read and the upkeep's.
    w.homeserver.fail_writes(4, false);
    let unchanged = shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), &held);
    let owed = unchanged.app_record_owed.unwrap();
    assert!(owed.contains("Homeserver"), "{owed}");
}

#[tokio::test]
async fn test_own_record_unknown_store_result_told_at_a_receive_that_skips_the_upkeep() {
    // The store can't tell whether the wallet's last write of its record was applied. Its
    // account is then gone at the switch: the receive returns at once, before the upkeep,
    // and still tells the host the record is owed until the app is loaded again.
    use std::sync::atomic::{AtomicBool, Ordering};
    let w = World::new();
    let base = crate::common::store();
    let faulty = std::sync::Arc::new(super::UnknownOnce {
        inner: std::sync::Arc::clone(&base),
        armed: AtomicBool::new(false),
    });
    let wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&faulty)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let wallet_id = wallet.app_id();
    drop(wallet);
    let transport = std::sync::Arc::new(crate::erasure::ErasedSwitch {
        inner: std::sync::Arc::clone(&w.router),
        erased: AtomicBool::new(false),
    });
    let mut wallet = w
        .load_with_transport(
            &w.alice,
            std::sync::Arc::clone(&faulty)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
            wallet_id,
            std::sync::Arc::clone(&transport)
                as std::sync::Arc<dyn pubky_switch_client::transport::Transport>,
        )
        .await;
    wallet.receive(None).await.unwrap();
    w.rail
        .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
    faulty.armed.store(true, Ordering::SeqCst);
    assert!(wallet.update_own_record().await.is_err());
    transport.erased.store(true, Ordering::SeqCst);
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    for _ in 0..4 {
        assert!(wallet.receive(None).await.is_err());
        w.clock.advance_ms(6 * crate::common::HOUR_MS);
    }
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.account_gone, "{summary:?}");
    let owed = summary.app_record_owed.unwrap();
    assert!(owed.contains("loaded again"), "{owed}");
}

#[tokio::test]
async fn test_own_record_missing_copy_on_shared_keys_told_until_written_back() {
    // Alice's wallet runs on two devices that share its keys, and the record's copy on the
    // Homeserver goes missing: another device may have deleted it to leave, so the client
    // writes it back only once the switch shows the app current. The second device's read
    // waits for a data epoch's scope key, so it reads no feed to its end and shows nothing
    // current: the receive tells the host the copy is owed. Once the key comes, the next
    // receive reads the feed, writes the copy back and clears that.
    let w = World::new();
    let mut first = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .request()
                .pay()
                .build()
                .unwrap(),
            super::publishing(),
            crate::common::store(),
        )
        .await;
    first.receive(None).await.unwrap();
    let backup = first.backup().await.unwrap();
    let mut second = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        super::publishing(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // The host marks the device as one of several sharing the app's keys.
    second.app_mut().set_multi_device(true).await.unwrap();
    second.receive(None).await.unwrap();
    let path = pubky_switch_core::records::app_record_path(&second.app_id());
    let alice = w.alice_id();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    w.homeserver.remove(alice, &path);
    w.clock
        .advance_ms(pubky_switch_client::app::PUBLISHED_CHECK_MS + 1);
    let summary = second.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let owed = summary.app_record_owed.unwrap();
    assert!(owed.contains("missing"), "{owed}");
    assert!(w.homeserver.file(alice, &path).is_none());
    crate::certificate::store_bundle(&w, second.app_id(), &second.app().cert().clone());
    let mut cleared = None;
    for _ in 0..3 {
        let summary = second.receive(None).await.unwrap();
        if !summary.scope_keys_awaited {
            cleared = Some(summary);
            break;
        }
    }
    let summary = cleared.expect("the key comes");
    assert_eq!(summary.app_record_owed, None, "{summary:?}");
    assert!(w.homeserver.file(alice, &path).is_some());
}
