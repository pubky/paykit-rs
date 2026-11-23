//! How often a list read binds a record: once per record it lists. Each bind checks every
//! stored message the record holds (Paykit wire 5.3), so a list that bound a record again
//! for its periods, its kept stops or its pause would cost a multiple of that.

use paykit_sdk::{PayOptions, PayOutcome, ReceiptsOf, RequestStatus};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, World};

#[tokio::test]
async fn test_list_reads_bind_each_record_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut requests = Vec::new();
    for value in ["1", "2", "3", "4", "5"] {
        requests.push(request_from(&mut shop, w.alice_id(), draft(value, w.clock.now_ms())).await);
    }
    wallet.receive(None).await.unwrap();
    // Three are paid and receipted; two stay open.
    for request in &requests[..3] {
        let paid = wallet
            .pay_request(request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    }
    shop.receive(None).await.unwrap();
    for sale in shop.payee_activity().await.unwrap() {
        if sale.attempt_id.is_some() {
            assert!(shop.confirm_payment(&sale.record).await.unwrap().is_some());
        }
    }
    wallet.receive(None).await.unwrap();

    let before = wallet.activity_binds();
    let shown = wallet.requests().await.unwrap();
    assert_eq!(
        wallet.activity_binds() - before,
        5,
        "one bind per payer record"
    );
    let paid = shown
        .iter()
        .filter(|view| view.status == RequestStatus::Paid)
        .count();
    assert_eq!((shown.len(), paid), (5, 3), "{shown:?}");

    let before = wallet.activity_binds();
    assert_eq!(wallet.receipts(ReceiptsOf::All).await.unwrap().len(), 3);
    assert_eq!(wallet.activity_binds() - before, 5);

    let before = shop.activity_binds();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 5);
    assert_eq!(
        shop.activity_binds() - before,
        5,
        "one bind per payee record"
    );
}
