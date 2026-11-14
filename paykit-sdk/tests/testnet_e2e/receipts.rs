//! Receipt Access delivery status across the drain paths that send it.

use paykit_lib::PaymentReference;
use paykit_sdk::{ReceiptDraftBuilder, ReceiptIssuanceStatus, ReceiptIssuanceView};

use crate::harness::{linked_two_party, TestUser, TwoParty};

/// Issue a receipt from Alice to Bob and check it waits in the queue.
async fn issue_receipt_to_bob(pair: &TwoParty) -> ReceiptIssuanceView {
    let draft = ReceiptDraftBuilder::from_payment_reference(
        PaymentReference::new("invoice-2026-0001").unwrap(),
    )
    .with_new_receipt_id()
    .build()
    .unwrap();
    let issuance = pair
        .alice
        .sdk
        .issue_receipt(pair.bob.public_key.clone(), draft)
        .await
        .expect("issuing the Receipt should store it and queue Receipt Access");
    assert_eq!(issuance.status, ReceiptIssuanceStatus::AccessQueued);
    assert_eq!(
        issued_receipt(&pair.alice, &issuance.receipt_id)
            .await
            .status,
        ReceiptIssuanceStatus::AccessQueued
    );
    issuance
}

async fn issued_receipt(user: &TestUser, receipt_id: &str) -> ReceiptIssuanceView {
    user.sdk
        .issued_receipts()
        .await
        .expect("issued receipts should list")
        .into_iter()
        .find(|view| view.receipt_id == receipt_id)
        .expect("the issued receipt should be listed")
}

/// Bob receives Receipt Access and Alice reports it sent, in both list calls.
async fn assert_access_sent(pair: &TwoParty, issuance: &ReceiptIssuanceView) {
    let all = issued_receipt(&pair.alice, &issuance.receipt_id).await;
    assert_eq!(all.status, ReceiptIssuanceStatus::AccessSent);
    assert!(all.access_sent_at.is_some());
    assert!(all.last_error.is_none());
    let to_bob = pair
        .alice
        .sdk
        .issued_receipts_to(&pair.bob.public_key)
        .await
        .unwrap();
    assert_eq!(to_bob, vec![all]);

    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("Bob should receive Receipt Access");
    pair.bob
        .sdk
        .retrieve_receipt(pair.alice.public_key.clone(), &issuance.receipt_id)
        .await
        .expect("Bob should fetch and decrypt the Receipt");
}

#[tokio::test]
async fn test_receipt_access_sent_by_process_outbound_private_messages() {
    let pair = linked_two_party().await;
    let issuance = issue_receipt_to_bob(&pair).await;

    let report = pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("processing the outbound queue should succeed");
    assert!(report.sent.contains(&issuance.outbound_message_id.unwrap()));

    assert_access_sent(&pair, &issuance).await;
}

#[tokio::test]
async fn test_receipt_access_sent_by_process_pending_private_messages() {
    let pair = linked_two_party().await;
    let issuance = issue_receipt_to_bob(&pair).await;

    let reports = pair
        .alice
        .sdk
        .process_pending_private_messages()
        .await
        .expect("processing pending private messages should succeed");
    assert!(reports.iter().all(|report| report.error.is_none()));

    assert_access_sent(&pair, &issuance).await;
}

#[tokio::test]
async fn test_receipt_access_sent_by_private_payment_list_sync() {
    let pair = linked_two_party().await;
    let issuance = issue_receipt_to_bob(&pair).await;

    // The list sync drains every pending queue; its report covers lists only.
    pair.alice
        .sdk
        .sync_contact_private_payment_lists_and_process_outbound(false)
        .await
        .expect("syncing Private Payment Lists should succeed");

    assert_access_sent(&pair, &issuance).await;
}
