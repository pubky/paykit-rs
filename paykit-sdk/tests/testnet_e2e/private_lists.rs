use paykit_sdk::{OutboundPrivateMessageStatus, PaykitSdkError};

use crate::harness::{linked_two_party, receiving_detail, two_party};

#[tokio::test]
async fn test_private_payment_list_roundtrip_between_linked_peers() {
    let pair = linked_two_party().await;
    pair.alice
        .adapter
        .set_private_details(vec![receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let queued = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect("enqueue should succeed for a linked peer");
    assert_eq!(queued.status, OutboundPrivateMessageStatus::Pending);

    let send_report = pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("processing the outbound queue should succeed");
    assert_eq!(send_report.sent, vec![queued.outbound_message_id]);
    assert!(send_report.failed.is_empty());

    let intake = pair
        .bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("receiving private messages should succeed");
    assert!(!intake.stream_item_ids.is_empty());
    assert!(intake.event_conflicts.is_empty());

    let view = pair
        .bob
        .sdk
        .current_private_payment_list(&pair.alice.public_key)
        .await
        .expect("reading the Private Payment List should succeed")
        .expect("a valid list should be present after receive");
    assert_eq!(
        view.payment_endpoints
            .get("btc-lightning-bolt11")
            .map(String::as_str),
        Some("ln-private-alice")
    );
    assert!(view.latest_stream_item_id.is_some());
}

#[tokio::test]
async fn test_enqueue_private_payment_list_without_link_fails() {
    let pair = two_party().await;
    pair.alice
        .adapter
        .set_private_details(vec![receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let err = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect_err("enqueue without an Encrypted Link must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired(_)),
        "unexpected error: {err:?}"
    );
}
