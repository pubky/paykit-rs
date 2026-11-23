//! A merchant updates the reusable endpoint it shares with a chosen contact.

use crate::common::{btc, endpoint, World};
use paykit_lib::derive::natural_key;
use paykit_lib::records::ListPublicationRecord;
use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{PayOutcome, Paykit, UnrequestedPayment};
use pubky_switch_client::app::{
    Collection, KeptReason, RecordApplication, RecordRead, RecordTarget,
};
use pubky_switch_core::ident::Policy;

pub(super) fn target(shop: &Paykit, recipient: &[u8; 32], generation: u64) -> RecordTarget {
    let policy = Policy::new("paykit_request", &["paykit_request"]).permanent(Some(generation));
    RecordTarget::new(
        Collection::new("private_list_publications", policy),
        &natural_key::list_publication(&shop.app_id(), recipient, generation),
    )
}

pub(super) fn offered(
    offer: &str,
) -> std::collections::BTreeMap<paykit_lib::PaymentEndpointIdentifier, PaymentEndpointPayload> {
    [(
        endpoint("btc-lightning-bolt12"),
        PaymentEndpointPayload::new(offer).unwrap(),
    )]
    .into_iter()
    .collect()
}

#[tokio::test]
async fn test_publication_updates_one_contacts_payable_list() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let generation = shop.app().own_identity().generation();
    let target = target(&shop, &w.alice_id(), generation);
    let key = shop.app().record_key(&target).unwrap();
    for (counter, offer) in [(1, "lno1first"), (2, "lno1second")] {
        let previous = shop.app_mut().read_record_fresh(&target).await.unwrap();
        if counter == 1 {
            assert!(matches!(
                previous,
                RecordRead::Kept {
                    record: None,
                    reason: KeptReason::NotFound
                }
            ));
        } else {
            let RecordRead::Fresh(record) = previous else {
                panic!("counter not current: {previous:?}")
            };
            let before = ListPublicationRecord::read(record.content.value().unwrap()).unwrap();
            assert_eq!(record.policy, target.collection.policy);
            assert_eq!(before.counter, counter - 1);
        }
        shop.set_list_endpoints(offered(offer)).await.unwrap();
        shop.publish_list_to(w.alice_id()).await.unwrap();
        wallet.receive(None).await.unwrap();
        let result = wallet
            .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
            .await
            .outcome
            .unwrap();
        assert!(matches!(result, PayOutcome::Paid { .. }), "{result:?}");
        assert_eq!(
            w.rail.selections().last().unwrap().0[0].payload.as_str(),
            offer
        );
        assert_eq!(
            w.record_writes(&w.bob, &key),
            usize::try_from(counter).unwrap()
        );
    }
}

#[tokio::test]
async fn test_publication_restored_withdrawal_never_admits_its_recipient() {
    use crate::common::store;
    use paykit_sdk::PaykitConfig;
    use pubky_switch_client::app::Delivery;

    let w = World::new();
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    let recipient = w.alice_id();
    let backup = shop.backup().await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(recipient).await.unwrap();
    let target = target(&shop, &recipient, 1);
    let key = shop.app().record_key(&target).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let first = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(matches!(
        shop.app().delivery(&first.msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    let admitted = shop.app_mut().allow_list().await.unwrap().conversations;
    assert!(admitted.iter().any(|entry| entry.pubky == recipient));
    shop.app_mut()
        .remove_conversation(&recipient)
        .await
        .unwrap();
    drop(shop);

    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let withdrawal = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(withdrawal.counter, first.counter + 1);
    assert!(matches!(
        shop.app().delivery(&withdrawal.msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert!(shop
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .conversations
        .is_empty());
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let retry = shop
        .app_mut()
        .flush_message(&withdrawal.msg_id)
        .await
        .unwrap();
    assert!(
        matches!(retry, Some(Delivery::Accepted { .. })),
        "{retry:?}"
    );
    assert!(shop
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .conversations
        .is_empty());
}

#[tokio::test]
async fn test_publication_restore_lists_former_and_current_recipients() {
    use crate::common::{store, BOB_ORIGIN};
    use paykit_sdk::PaykitConfig;
    use pubky_switch_core::records::Inbox;
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut other = w
        .join(
            &w.carol,
            "other",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    other.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    shop.set_list_endpoints(offered("lno1former"))
        .await
        .unwrap();
    shop.publish_list_to(w.alice_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let former = target(&shop, &w.alice_id(), 1);
    let former_key = shop.app().record_key(&former).unwrap();
    let changed = w.bob.switch().change_key_into([74; 32]);
    w.router.add_switch(BOB_ORIGIN, &changed);
    let current = w
        .bob
        .sign_next(|body| {
            body.generation = 2;
            body.inbox = Inbox::new(BOB_ORIGIN, changed.service_key());
        })
        .unwrap();
    changed.deliver_identity(&current).unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    let carol = w.carol.identity().public();
    shop.publish_list_to(carol).await.unwrap();
    let current = target(&shop, &carol, 2);
    let current_key = shop.app().record_key(&current).unwrap();
    assert!(changed.record(&former_key).is_some());
    assert!(changed.record(&current_key).is_some());
    drop(shop);

    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.app_mut().refresh_identity().await.unwrap();
    restored.app_mut().follow_provider().await.unwrap();
    assert_eq!(restored.app().own_identity().generation(), 2);
    assert!(restored
        .app()
        .shared_record(&former_key)
        .await
        .unwrap()
        .is_none());
    let page = restored
        .app_mut()
        .list_collection(&current.collection)
        .await
        .unwrap();
    assert_eq!(page.records, 2, "{page:?}");
    assert!(!page.more);
    let known = restored
        .app()
        .records_of("private_list_publications")
        .await
        .unwrap();
    assert!(
        known
            .iter()
            .any(|r| r.key == current_key && r.policy == current.collection.policy),
        "{known:?}"
    );
    assert!(!known.iter().any(|r| r.key == former_key));
    let withdrawal = target(&restored, &w.alice_id(), 2);
    let withdrawal_key = restored.app().record_key(&withdrawal).unwrap();
    assert!(changed.record(&withdrawal_key).is_none());
    restored.receive(None).await.unwrap();
    let known = restored
        .app()
        .records_of("private_list_publications")
        .await
        .unwrap();
    assert!(
        known
            .iter()
            .any(|r| r.key == former_key && r.policy == former.collection.policy),
        "{known:?}"
    );
    let RecordRead::Fresh(latest) = restored
        .app_mut()
        .read_record_fresh(&withdrawal)
        .await
        .unwrap()
    else {
        panic!("no current withdrawal");
    };
    let withdrawn = ListPublicationRecord::read(latest.content.value().unwrap()).unwrap();
    assert_eq!((withdrawn.generation, withdrawn.counter), (2, 1));
    assert_eq!(withdrawn.recipient, w.alice_id());
    let sent = restored
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    let sent = sent.iter().find(|m| m.msg_id == withdrawn.msg_id).unwrap();
    assert_eq!(sent.plaintext.hash(), withdrawn.message_hash);
    assert!(sent.accepted_at_ms.is_some());
    let signed = pubky_switch_core::message::Message::parse(
        sent.plaintext.json(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    let list = paykit_lib::bodies::PrivatePaymentList::from_body(&signed.body).unwrap();
    assert_eq!(list.version, paykit_lib::ListVersion::new(2, 1).unwrap());
    assert!(list.payment_endpoints.is_empty());
    wallet.receive(None).await.unwrap();
    let stopped = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome;
    assert!(
        matches!(
            stopped.as_ref().err().and_then(|e| e.refusal()),
            Some(paykit_sdk::Refusal::WaitingForList)
        ),
        "{stopped:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    let again = restored.backup().await.unwrap();
    drop(restored);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &again,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    let read = restored
        .app_mut()
        .read_record_fresh(&withdrawal)
        .await
        .unwrap();
    let RecordRead::Fresh(second) = read else {
        panic!("no second withdrawal: {read:?}");
    };
    let second = ListPublicationRecord::read(second.content.value().unwrap()).unwrap();
    assert_eq!((second.generation, second.counter), (2, 2));
    assert_ne!(second.msg_id, withdrawn.msg_id);
    let before = changed.applied().len();
    restored
        .set_list_endpoints(offered("lno1private"))
        .await
        .unwrap();
    assert_eq!(
        changed.applied().len(),
        before,
        "a withdrawal-only recipient was enrolled"
    );
}

#[tokio::test]
async fn test_publication_lost_first_race_reads_the_winning_counter() {
    use crate::common::{store, ReadHook};
    use paykit_sdk::{AppScopes, PaykitConfig};
    use pubky_switch_core::http::ErrorCode;
    use std::sync::Arc;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let hook = Arc::new(ReadHook::new(w.router.clone()));
    let mut shop = w
        .join_through(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            store(),
            hook.clone(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let mut other = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    other.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1new")).await.unwrap();
    other
        .set_list_endpoints(offered("lno1winning"))
        .await
        .unwrap();
    let recipient = w.alice_id();
    let target = target(&shop, &recipient, 1);
    let key = shop.app().record_key(&target).unwrap();
    hook.after_read(
        &format!("records/{}", pubky_switch_core::enc::b64(&key)),
        move || {
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(other.publish_list_to(recipient))
                    .unwrap();
            })
            .join()
            .unwrap();
        },
    );
    let lost = shop.publish_list_to(recipient).await.unwrap_err();
    assert!(hook.fired());
    assert_eq!(lost.code(), Some(ErrorCode::Conflict), "{lost}");
    assert!(lost.kept_batch().is_none());
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    shop.publish_list_to(recipient).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    wallet.receive(None).await.unwrap();
    let result = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }), "{result:?}");
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1new"
    );
}

#[tokio::test]
async fn test_publication_lost_answer_settles_the_original_before_replacing_its_list() {
    use crate::common::store;
    use paykit_sdk::{AppScopes, PaykitConfig, Refusal};
    use pubky_switch_client::app::{Delivery, KeptBatchStatus};
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let id = shop.app_id();
    let target = target(&shop, &w.alice_id(), 1);
    let key = shop.app().record_key(&target).unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let first = shop.publish_list_to(w.alice_id()).await.unwrap_err();
    let batch = first.kept_batch().unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    let owed = shop.app().queued_messages("paykit").await.unwrap();
    let original = owed
        .iter()
        .find(|m| m.kept_batch == Some(batch))
        .unwrap()
        .msg_id
        .clone();
    assert!(
        matches!(shop.app().delivery(&original).await.unwrap(), Some(Delivery::Kept {batch_id}) if batch_id == batch)
    );
    drop(shop);
    let mut shop = w.load_on_clock(&w.bob, storage, id, w.clock.clone()).await;
    let still_owed = shop.publish_list_to(w.alice_id()).await.unwrap_err();
    assert!(
        matches!(still_owed.refusal(), Some(Refusal::NotNow(_))),
        "{still_owed}"
    );
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    shop.set_list_endpoints(offered("lno1changed"))
        .await
        .unwrap();
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(KeptBatchStatus::Kept { .. })
    ));
    shop.app_mut().receive(None).await.unwrap();
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(KeptBatchStatus::Applied { .. })
    ));
    shop.app_mut().flush_outbox().await.unwrap();
    let original_view = shop.app().queued_messages("paykit").await.unwrap();
    assert!(
        original_view
            .iter()
            .any(|m| m.msg_id == original && !m.may_send),
        "{original_view:?}"
    );
    shop.publish_list_to(w.alice_id()).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    wallet.receive(None).await.unwrap();
    let result = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }), "{result:?}");
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1changed"
    );
}

#[tokio::test]
async fn test_publication_expired_original_batch_still_waits_at_the_same_counter() {
    use pubky_switch_client::app::{KeptBatchDrop, KeptBatchStatus};
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut shop = w.shop().await;
    let _wallet = w.wallet("wallet").await;
    let recipient = w.alice_id();
    shop.app_mut().invite(&recipient).await.unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    let target = target(&shop, &recipient, 1);
    let key = shop.app().record_key(&target).unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let lost = shop.publish_list_to(recipient).await.unwrap_err();
    let batch = lost.kept_batch().unwrap();
    let owned = shop.app().queued_messages("paykit").await.unwrap();
    let original = owned.iter().find(|m| m.kept_batch == Some(batch)).unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    w.clock.advance_ms(31 * 24 * crate::common::HOUR_MS);
    shop.app_mut().receive(None).await.unwrap();
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(KeptBatchStatus::Dropped {
            reason: KeptBatchDrop::Uncertain,
            ..
        })
    ));
    let RecordRead::Fresh(held) = shop.app_mut().read_record_fresh(&target).await.unwrap() else {
        panic!("the signed counter is still current")
    };
    let held = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(held.msg_id, original.msg_id);
    assert_eq!(held.message_hash, original.hash);
    shop.set_list_endpoints(offered("lno1replacement"))
        .await
        .unwrap();
    let result = shop.publish_list_to(recipient).await;
    assert!(
        matches!(
            result.as_ref().err().and_then(|e| e.refusal()),
            Some(paykit_sdk::Refusal::NotNow(_))
        ),
        "{result:?}"
    );
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert!(shop
        .app()
        .sent_to(&recipient, 0, u64::MAX)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_publication_imported_original_settles_before_a_new_generation_list() {
    use pubky_switch_client::app::{AppEvent, KeptBatchStatus};
    use pubky_switch_client::testing::Fault;
    use pubky_switch_core::records::Inbox;
    const NEXT: &str = "https://moved-merchant.example";
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let recipient = w.alice_id();
    shop.app_mut().invite(&recipient).await.unwrap();
    let first_key = shop
        .app()
        .record_key(&target(&shop, &recipient, 1))
        .unwrap();
    shop.set_list_endpoints(offered("lno1old")).await.unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let lost = shop.publish_list_to(recipient).await.unwrap_err();
    let batch = lost.kept_batch().unwrap();
    assert_eq!(w.record_writes(&w.bob, &first_key), 1);
    shop.set_list_endpoints(offered("lno1afterimport"))
        .await
        .unwrap();
    let next = w.bob.switch().import_into_at([85; 32], NEXT);
    w.router.add_switch(NEXT, &next);
    let changed = w
        .bob
        .sign_next(|body| {
            body.generation = 2;
            body.inbox = Inbox::new(NEXT, next.service_key());
        })
        .unwrap();
    next.deliver_identity(&changed).unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    let report = shop.app_mut().receive(None).await.unwrap();
    assert!(
        report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::KeptBatchApplied {batch_id} if *batch_id==batch)),
        "{:?}",
        report.events
    );
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(KeptBatchStatus::Applied { .. })
    ));
    assert_eq!(shop.app().own_identity().generation(), 2);
    let current = target(&shop, &recipient, 2);
    let key = shop.app().record_key(&current).unwrap();
    assert!(next.record(&key).is_none());
    shop.publish_list_to(recipient).await.unwrap();
    let RecordRead::Fresh(held) = shop.app_mut().read_record_fresh(&current).await.unwrap() else {
        panic!("no current publication")
    };
    let issued = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!((issued.generation, issued.counter), (2, 1));
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }));
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1afterimport"
    );
}

#[tokio::test]
async fn test_publication_restored_contact_needs_a_new_sharing_choice() {
    use crate::common::store;
    use paykit_sdk::{PaykitConfig, Refusal};

    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.set_list_endpoints(offered("lno1before"))
        .await
        .unwrap();
    shop.publish_list_to(w.alice_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    shop.withdraw_list_from(w.alice_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    drop(shop);

    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let current = target(&shop, &w.alice_id(), shop.app().own_identity().generation());
    let key = shop.app().record_key(&current).unwrap();
    let withdrawn = w.record_writes(&w.bob, &key);
    assert_eq!(withdrawn, 3, "one issue and both withdrawals");
    let before_reset = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome;
    assert!(
        matches!(
            before_reset.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::NoDestination(_))
        ),
        "{before_reset:?}"
    );
    shop.set_list_endpoints(offered("lno1after")).await.unwrap();
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome;
    assert!(
        matches!(
            held.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::NoDestination(_))
        ),
        "payment {held:?}, selected {:?}, counter writes {}",
        w.rail
            .selections()
            .last()
            .map(|s| s.0[0].payload.as_str().to_owned()),
        w.record_writes(&w.bob, &key)
    );
    assert_eq!(w.record_writes(&w.bob, &key), withdrawn);
    assert!(w.rail.all_executions().is_empty());

    shop.publish_list_to(w.alice_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), withdrawn + 1);
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1after"
    );
}

async fn interrupted_keep(after_commit: bool) {
    use crate::common::FailNextKeep;
    use paykit_sdk::{AppScopes, PaykitConfig};

    use pubky_switch_client::store::StateStore;
    use std::sync::Arc;

    let w = World::new();
    let disk = Arc::new(FailNextKeep::default());
    let storage = disk.clone() as Arc<dyn StateStore>;
    let mut shop = w
        .join_with(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.set_list_endpoints(
        [(
            endpoint("btc-lightning-bolt12"),
            paykit_lib::PaymentEndpointPayload::new("lno1unsent").unwrap(),
        )]
        .into_iter()
        .collect(),
    )
    .await
    .unwrap();
    let publisher = shop.app_id();
    let before = w.bob.switch().applied().len();
    if after_commit {
        disk.lose_keep_answer();
    } else {
        disk.fail_before_keep();
    }
    let err = shop.publish_list_to(w.alice_id()).await.unwrap_err();
    assert_eq!(err.kept_batch().is_some(), after_commit, "{err}");
    assert_eq!(w.bob.switch().applied().len(), before);
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, storage, publisher, w.clock.clone())
        .await;
    let kept = shop.app().queued_messages("paykit").await.unwrap();
    assert_eq!(kept.len(), usize::from(after_commit));
    if after_commit {
        let batch = kept[0].kept_batch.unwrap();
        assert_eq!(Some(batch), err.kept_batch());
        let pending = shop.publish_list_to(w.alice_id()).await.unwrap_err();
        assert!(
            matches!(pending.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
            "{pending}"
        );
        shop.app_mut().receive(None).await.unwrap();
        assert!(matches!(
            shop.app().kept_batch(batch).await.unwrap(),
            Some(pubky_switch_client::app::KeptBatchStatus::Applied { .. })
        ));
    }

    shop.receive(None).await.unwrap();
    shop.publish_list_to(w.alice_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }));
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1unsent"
    );
}

#[tokio::test]
async fn test_publication_resumes_when_its_first_batch_was_never_kept() {
    interrupted_keep(false).await;
}

#[tokio::test]
async fn test_publication_reloads_the_original_after_its_first_keep_answer_is_lost() {
    interrupted_keep(true).await;
}

#[tokio::test]
async fn test_publication_later_provider_refusal_does_not_release_the_lost_original() {
    use pubky_switch_client::app::{KeptBatchDrop, KeptBatchStatus};
    use pubky_switch_client::testing::{Fault, ScriptedSwitch};
    use pubky_switch_core::records::Inbox;
    const NEXT: &str = "https://recovered-merchant.example";
    let w = World::new();
    let mut shop = w.shop().await;
    let _wallet = w.wallet("wallet").await;
    let recipient = w.alice_id();
    shop.app_mut().invite(&recipient).await.unwrap();
    let old = shop
        .app()
        .record_key(&target(&shop, &recipient, 1))
        .unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let original = shop
        .publish_list_to(recipient)
        .await
        .unwrap_err()
        .kept_batch()
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &old), 1);
    assert!(matches!(
        shop.app().kept_batch(original).await.unwrap(),
        Some(KeptBatchStatus::Kept {
            sent_at_ms: Some(_),
            ..
        })
    ));
    let next = ScriptedSwitch::recovering(w.bob.identity().key(), [93; 32], w.clock.clone(), 2, 1)
        .unwrap();
    let doc = w
        .bob
        .sign_next(|b| {
            b.generation = 2;
            b.inbox = Inbox::new(NEXT, next.service_key());
            b.recovery_from = Some(1);
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    w.router.remove(crate::common::BOB_ORIGIN);
    w.router.add_switch(NEXT, &next);
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    assert!(matches!(shop.app().kept_batch(original).await.unwrap(),
        Some(KeptBatchStatus::Kept {wire_batch_id,sent_at_ms: None}) if wire_batch_id!=original));
    // The replacement provider cannot apply the counter write for generation one.
    for _ in 0..4 {
        let _report = shop.app_mut().receive(None).await;
    }
    let ended = shop.app().kept_batch(original).await.unwrap();
    assert!(
        matches!(
            ended,
            Some(KeptBatchStatus::Dropped {
                reason: KeptBatchDrop::Refused { .. } | KeptBatchDrop::Unsent,
                ..
            })
        ),
        "{ended:?}"
    );
    shop.set_list_endpoints(offered("lno1afterloss"))
        .await
        .unwrap();
    let issue = shop.publish_list_to(recipient).await;
    assert!(
        matches!(issue.as_ref().err().and_then(|e|e.refusal()),
        Some(paykit_sdk::Refusal::NotNow(reason)) if reason.contains("previous publication")),
        "{issue:?}"
    );
    let current = shop
        .app()
        .record_key(&target(&shop, &recipient, 2))
        .unwrap();
    assert!(next.record(&current).is_none());
}

#[tokio::test]
async fn test_publication_older_in_place_restore_retains_the_later_unresolved_recipient() {
    use crate::common::store;
    use paykit_sdk::{AppScopes, PaykitConfig, Refusal};
    use pubky_switch_client::testing::Fault;
    let w = World::new();
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let mut other = w
        .join(&w.carol, "other", AppScopes::new().pay().build().unwrap())
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&other.identity()).await.unwrap();
    other.app_mut().invite(&w.bob_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let recipient = w.alice_id();
    let first = shop
        .app()
        .record_key(&target(&shop, &recipient, 1))
        .unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let original = shop
        .publish_list_to(recipient)
        .await
        .unwrap_err()
        .kept_batch()
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &first), 1);
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, storage),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(shop.app().kept_batch(original).await.unwrap().is_none());
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1other")).await.unwrap();
    let held = shop.publish_list_to(recipient).await.unwrap_err();
    assert!(
        matches!(held.refusal(), Some(Refusal::NotNow(reason)) if reason.contains("previous publication")),
        "{held}"
    );
    assert_eq!(w.record_writes(&w.bob, &first), 1);
    shop.publish_list_to(other.identity()).await.unwrap();
    other.receive(None).await.unwrap();
    let paid = other
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1other"
    );
}

#[tokio::test]
async fn test_publication_requires_a_current_own_identity_after_reading_the_counter() {
    use crate::common::{store, ReadHook};
    use paykit_sdk::{AppScopes, PaykitConfig, Refusal};
    use std::sync::Arc;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let hook = Arc::new(ReadHook::new(w.router.clone()));
    let mut shop = w
        .join_through(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            store(),
            hook.clone(),
        )
        .await;
    let recipient = wallet.identity();
    let owner = shop.identity();
    shop.app_mut().invite(&recipient).await.unwrap();
    wallet.app_mut().invite(&owner).await.unwrap();
    shop.set_list_endpoints(offered("lno1current"))
        .await
        .unwrap();
    let held = w.homeserver.identity_doc(&owner).unwrap();
    let key = shop
        .app()
        .record_key(&target(&shop, &recipient, held.generation()))
        .unwrap();
    let clock = w.clock.clone();
    let homeserver = w.homeserver.clone();
    hook.after_read(
        &format!("records/{}", pubky_switch_core::enc::b64(&key)),
        move || {
            clock.advance_ms(60_000);
            homeserver.remove(owner, pubky_switch_core::records::IDENTITY_PATH);
        },
    );
    let denied = shop.publish_list_to(recipient).await.unwrap_err();
    assert!(hook.fired());
    assert!(
        matches!(denied.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{denied}"
    );
    assert_eq!(w.record_writes(&w.bob, &key), 0);
    assert!(shop
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    w.homeserver.publish(&held).unwrap();
    shop.publish_list_to(recipient).await.unwrap();
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(owner, btc("0.001")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }));
    assert_eq!(w.record_writes(&w.bob, &key), 1);
}

#[tokio::test]
async fn test_publication_restored_pending_batch_needs_its_matching_applied_write() {
    use crate::common::store;
    use paykit_sdk::{AppScopes, PaykitConfig, Refusal};
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let disk = store();
    let recipient = w.alice_id();
    let mut shop = w
        .join_with(
            &w.bob,
            "publisher",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            disk.clone(),
        )
        .await;
    let _wallet = w.wallet("wallet").await;
    shop.app_mut().invite(&recipient).await.unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    w.bob.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let batch = shop
        .publish_list_to(recipient)
        .await
        .unwrap_err()
        .kept_batch()
        .unwrap();
    let backup = shop.backup().await.unwrap();
    let mut elsewhere = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    elsewhere.receive(None).await.unwrap();
    elsewhere
        .set_list_endpoints(offered("lno1new"))
        .await
        .unwrap();
    let no_outcome = elsewhere.publish_list_to(recipient).await.unwrap_err();
    assert!(
        matches!(no_outcome.refusal(),Some(Refusal::NotNow(reason)) if reason.contains("previous publication")),
        "{no_outcome}"
    );
    drop(elsewhere);
    shop.app_mut().receive(None).await.unwrap();
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(pubky_switch_client::app::KeptBatchStatus::Applied { .. })
    ));
    let original = target(&shop, &recipient, 1);
    let key = shop.app().record_key(&original).unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, disk),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        w.record_writes(&w.bob, &key),
        2,
        "verified earlier application permits the restore withdrawal"
    );
    restored
        .set_list_endpoints(offered("lno1new"))
        .await
        .unwrap();
    assert_eq!(
        w.record_writes(&w.bob, &key),
        2,
        "restore did not renew sharing permission"
    );
    restored.publish_list_to(recipient).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
}

#[tokio::test]
async fn test_publication_observed_by_a_second_installation_survives_selected_restore() {
    use crate::common::store;
    use paykit_sdk::PaykitConfig;
    use pubky_switch_core::enc::b64;

    let w = World::new();
    let mut publisher = w.shop().await;
    let _wallet = w.wallet("wallet").await;
    let initial = publisher.backup().await.unwrap();
    let mut observer = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &initial,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    observer.receive(None).await.unwrap();
    let recipient = w.alice_id();
    let counter = target(&publisher, &recipient, 1);
    let key = publisher.app().record_key(&counter).unwrap();

    publisher
        .set_list_endpoints(offered("lno1observed"))
        .await
        .unwrap();
    publisher.publish_list_to(recipient).await.unwrap();
    let RecordRead::Fresh(current) = observer
        .app_mut()
        .read_record_fresh(&counter)
        .await
        .unwrap()
    else {
        panic!("the publisher's original issue must be current")
    };
    let issued = ListPublicationRecord::read(current.content.value().unwrap()).unwrap();
    let at = format!("lhi/{}", b64(&key));
    let history = observer
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_slice(&history).unwrap();
    let floor =
        ListPublicationRecord::read(&serde_json::to_vec(&stored["d"]["floor"]).unwrap()).unwrap();
    assert_eq!(floor, issued);
    // This installation only read the counter. It never prepared or applied this issue.
    let reservation = format!("lres/{}", b64(&recipient));
    assert!(observer
        .app()
        .protocol_get("paykit", reservation.as_bytes())
        .await
        .unwrap()
        .is_none());

    let selected = observer.backup().await.unwrap();
    drop(observer);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &selected,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored
            .app()
            .protocol_get("paykit", at.as_bytes())
            .await
            .unwrap()
            .unwrap(),
        history
    );
    assert!(restored.publication_conflicts().await.unwrap().is_empty());
    restored.receive(None).await.unwrap();
    let RecordRead::Fresh(current) = restored
        .app_mut()
        .read_record_fresh(&counter)
        .await
        .unwrap()
    else {
        panic!("the restored withdrawal must be current")
    };
    let withdrawal = ListPublicationRecord::read(current.content.value().unwrap()).unwrap();
    assert_eq!(withdrawal.counter, issued.counter + 1);
    assert_ne!(withdrawal.msg_id, issued.msg_id);
    assert_ne!(withdrawal.message_hash, issued.message_hash);
    restored
        .set_list_endpoints(offered("lno1unselected"))
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
}

#[tokio::test]
async fn test_publication_retained_report_remains_available_after_selling_scope_is_removed() {
    use paykit_sdk::AppScopes;
    use pubky_switch_client::app::LocalWrite;
    use pubky_switch_core::enc::b64;

    let w = World::new();
    let mut app = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut payer = w
        .join(&w.bob, "payer", AppScopes::new().pay().build().unwrap())
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let contact = w.bob_id();
    let key = app.app().record_key(&target(&app, &contact, 1)).unwrap();
    let at = format!("lhi/{}", b64(&key));
    app.set_list_endpoints(offered("lno1first")).await.unwrap();
    app.publish_list_to(contact).await.unwrap();
    let first = app.app().shared_record(&key).await.unwrap().unwrap();
    app.set_list_endpoints(offered("lno1second")).await.unwrap();
    let second = app.app().shared_record(&key).await.unwrap().unwrap();
    let first_value: serde_json::Value =
        serde_json::from_slice(first.content.value().unwrap()).unwrap();
    let second_value: serde_json::Value =
        serde_json::from_slice(second.content.value().unwrap()).unwrap();
    // Supply the existing owner-local warning about these two genuine issues. Reading or
    // acknowledging it requires no shared-counter read and never changes their floor.
    let mut state: serde_json::Value = serde_json::from_slice(
        &app.app()
            .protocol_get("paykit", at.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    state["d"]["conflict"] = serde_json::json!({
        "application_hash": b64(&[44;32]), "other_applications": false, "held": second_value, "refused": first_value,
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: at.as_bytes().to_vec(),
                value: serde_json::to_vec(&state).unwrap(),
            }],
        )
        .await
        .unwrap();
    let report = app.publication_conflicts().await.unwrap().pop().unwrap();
    let old = app.app().cert().clone();
    let narrowed = crate::certificate::renew(
        &w,
        app.app_id(),
        &AppScopes::new().contacts().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, app.app_id(), &old, &narrowed);
    w.clock.advance_ms(1_000);
    app.receive(None).await.unwrap();
    assert_eq!(app.app().cert().hash(), narrowed.hash());
    assert!(!app.app().cert().holds_scope("paykit_request"));
    assert_eq!(
        app.publication_conflicts().await.unwrap(),
        vec![report.clone()]
    );
    app.acknowledge_publication_conflict(&report).await.unwrap();
    assert!(app.publication_conflicts().await.unwrap().is_empty());
    let after: serde_json::Value = serde_json::from_slice(
        &app.app()
            .protocol_get("paykit", at.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(after["d"]["floor"], state["d"]["floor"]);
    let refusal = app
        .set_list_endpoints(offered("lno1withoutauthority"))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal.refusal(), Some(paykit_sdk::Refusal::ScopeMissing(scope)) if scope == "paykit_request"),
        "{refusal}"
    );
    assert_eq!(w.record_writes(&w.alice, &key), 2);
}

#[tokio::test]
async fn test_publication_labelled_seller_can_ask_and_pay_as_a_payer() {
    use paykit_lib::{kinds::Kind, Label};
    use paykit_sdk::{AppScopes, PayOptions, Refusal};
    use pubky_switch_client::app::AppEvent;

    let w = World::new();
    let label = Label::parse("shop1").unwrap();
    let mut payer = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new()
                .pay()
                .request_label(&label)
                .build()
                .unwrap(),
        )
        .await;
    let mut publisher = w.shop().await;
    let mut shop = w.labelled_shop("seller-wallet", "shop1").await;
    let asks = Kind::PrivatePaymentListRequest.as_str();
    let accepts = |app: &Paykit, kind: &str| {
        pubky_switch_core::records::AppRecord::verify(app.app().record(), &app.identity())
            .unwrap()
            .body()
            .accepts
            .iter()
            .any(|name| name == kind)
    };
    assert!(accepts(&publisher, asks));
    assert!(!accepts(&shop, asks));
    assert!(!accepts(&payer, asks));
    assert!(accepts(&payer, Kind::PrivatePaymentList.as_str()));
    assert!(accepts(&shop, Kind::PaymentProof.as_str()));
    let prior = shop.app().queued_messages("paykit").await.unwrap();
    assert!(shop.set_list_endpoints(offered("lno1shop")).await.is_err());
    assert!(shop.publish_list_to(w.alice_id()).await.is_err());
    assert_eq!(
        shop.app().queued_messages("paykit").await.unwrap().len(),
        prior.len()
    );
    let key = publisher
        .app()
        .record_key(&target(&shop, &w.alice_id(), 1))
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 0);

    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    publisher.app_mut().invite(&w.alice_id()).await.unwrap();
    let payment = || {
        UnrequestedPayment::new(w.bob_id(), btc("0.001"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
    };
    let refusal = payer
        .pay_without_request(payment())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refusal.refusal(), Some(Refusal::WaitingForList)),
        "{refusal}"
    );
    let received =
        |e: &AppEvent| matches!(e, AppEvent::MessageReceived { kind, .. } if kind == asks);
    assert!(publisher
        .receive(None)
        .await
        .unwrap()
        .report
        .events
        .iter()
        .any(received));
    assert!(!shop
        .receive(None)
        .await
        .unwrap()
        .report
        .events
        .iter()
        .any(received));
    publisher
        .set_list_endpoints(offered("lno1plain"))
        .await
        .unwrap();
    publisher.publish_list_to(w.alice_id()).await.unwrap();
    payer.receive(None).await.unwrap();
    let result = payer.pay_without_request(payment()).await.outcome.unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }), "{result:?}");
    assert_eq!(
        w.rail.selections().last().unwrap().0[0].payload.as_str(),
        "lno1plain"
    );
}

#[tokio::test]
async fn test_publication_restored_while_labelled_discovers_a_former_recipient_when_plain_again() {
    use crate::common::store;
    use paykit_lib::Label;
    use paykit_sdk::{AppScopes, PaykitConfig};
    use pubky_switch_client::app::Delivery;

    let w = World::new();
    let labelled = AppScopes::new()
        .request_label(&Label::parse("shop1").unwrap())
        .build()
        .unwrap();
    let plain = AppScopes::new().request().build().unwrap();
    let mut app = w.join(&w.alice, "wallet", labelled.clone()).await;
    let mut buyer = w
        .join(&w.bob, "payer", AppScopes::new().pay().build().unwrap())
        .await;
    let backup = app.backup().await.unwrap();
    let renew = |app: &Paykit, requested| {
        let old = app.app().cert().clone();
        let cert = crate::certificate::renew(&w, app.app_id(), requested, 1_000);
        crate::certificate::store_bundle(&w, app.app_id(), &cert);
        crate::certificate::list(&w, app.app_id(), &old, &cert);
        w.clock.advance_ms(1_000);
        cert
    };
    let authorized = renew(&app, &plain);
    app.receive(None).await.unwrap();
    assert_eq!(app.app().cert().hash(), authorized.hash());
    let counter = target(&app, &w.bob_id(), 1);
    let key = app.app().record_key(&counter).unwrap();
    app.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    app.publish_list_to(w.bob_id()).await.unwrap();
    let first = app.app().shared_record(&key).await.unwrap().unwrap();
    let issue = ListPublicationRecord::read(first.content.value().unwrap()).unwrap();
    assert!(matches!(
        app.app().delivery(&issue.msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    let limited = renew(&app, &labelled);
    app.receive(None).await.unwrap();
    assert_eq!(app.app().cert().hash(), limited.hash());
    buyer.app_mut().invite(&w.alice_id()).await.unwrap();
    let queued = app.app().queued_messages("paykit").await.unwrap();
    assert!(queued
        .iter()
        .any(|entry| entry.msg_id == issue.msg_id && !entry.may_send));
    assert!(matches!(
        app.app_mut().flush_message(&issue.msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert!(app
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(w.record_writes(&w.alice, &key), 1);
    drop(app);

    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), limited.hash());
    assert_eq!(w.record_writes(&w.alice, &key), 1);
    let authorized = renew(&restored, &plain);
    let grant = restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), authorized.hash());
    let epoch = restored.app().own_identity().body().data_epoch;
    if restored.app().scope_key(epoch, "paykit_request").is_none() {
        assert!(grant.scope_keys_awaited);
        assert_eq!(w.record_writes(&w.alice, &key), 1);
        assert!(restored
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap()
            .is_empty());
        w.clock.advance_ms(1_000);
        restored.receive(None).await.unwrap();
    }
    assert_eq!(w.record_writes(&w.alice, &key), 2);
    let record = restored.app().shared_record(&key).await.unwrap().unwrap();
    let withdrawal = ListPublicationRecord::read(record.content.value().unwrap()).unwrap();
    assert_eq!(withdrawal.counter, issue.counter + 1);
    let delivered = restored
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(delivered[0].plaintext.json()).unwrap();
    assert_eq!(body["msg_id"], withdrawal.msg_id);
    assert_eq!(body["body"]["payment_endpoints"], serde_json::json!({}));
}

async fn request_list(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    app_id: impl Into<Option<[u8; 16]>>,
    held_version: Option<paykit_lib::ListVersion>,
) {
    let id = send_list_request(wallet, shop, app_id, held_version).await;
    assert!(shop.receive(None).await.unwrap().applied.contains(&id));
}

async fn send_list_request(
    wallet: &mut Paykit,
    shop: &Paykit,
    app_id: impl Into<Option<[u8; 16]>>,
    held_version: Option<paykit_lib::ListVersion>,
) -> String {
    let body = paykit_lib::bodies::PrivatePaymentListRequest {
        app_id: app_id.into(),
        held_version,
    }
    .to_body()
    .unwrap();
    let (id, delivery) = wallet
        .app_mut()
        .send(pubky_switch_client::app::OutgoingMessage::new(
            shop.identity(),
            paykit_lib::Kind::PrivatePaymentListRequest.as_str(),
            body,
        ))
        .await
        .unwrap();
    assert!(matches!(
        delivery,
        pubky_switch_client::app::Delivery::Accepted { .. }
    ));
    id
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Published {
    version: paykit_lib::ListVersion,
    id: String,
    hash: [u8; 32],
    repeats: usize,
    withdrawn: bool,
}

async fn published(shop: &Paykit, wallet: &Paykit) -> Vec<Published> {
    let mut issues = Vec::new();
    for sent in shop
        .app()
        .sent_to(&wallet.identity(), 0, u64::MAX)
        .await
        .unwrap()
    {
        if sent.kind != paykit_lib::Kind::PrivatePaymentList.as_str() {
            continue;
        }
        let signed = pubky_switch_core::message::Message::parse(
            sent.plaintext.json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )
        .unwrap();
        let list = paykit_lib::bodies::PrivatePaymentList::from_body(&signed.body).unwrap();
        issues.push(Published {
            version: list.version,
            id: sent.msg_id,
            hash: sent.plaintext.hash(),
            repeats: sent.resent_at_ms.len(),
            withdrawn: list.payment_endpoints.is_empty(),
        });
    }
    issues.sort_by_key(|issue| issue.version);
    issues
}

#[derive(Clone, Copy, Debug)]
enum RequestStanding {
    Blocked,
    Flagged,
    Unknown,
}

async fn check_request_intake(standing: RequestStanding, restored: bool) {
    use crate::common::ListsStore;
    use paykit_sdk::{AppScopes, PaykitConfig};
    use std::sync::Arc;

    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            lists.clone(),
        )
        .await;
    let sender = wallet.identity();
    shop.app_mut().invite(&sender).await.unwrap();
    let lists = if restored {
        let backup = shop.backup().await.unwrap();
        let fresh = Arc::new(ListsStore::default());
        drop(shop);
        shop = Paykit::restore(
            w.env(&w.bob, fresh.clone()),
            PaykitConfig::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        fresh
    } else {
        shop.set_list_endpoints(offered("lno1chosen"))
            .await
            .unwrap();
        shop.publish_list_to(sender).await.unwrap();
        lists
    };
    shop.receive(None).await.unwrap();
    let before = shop.app().protocol_scan("paykit", b"lrec/").await.unwrap();
    let sent = published(&shop, &wallet).await;
    assert_eq!(before.len(), usize::from(!restored));
    assert_eq!(sent.len(), before.len());
    let version = paykit_lib::ListVersion::new(1, 1).unwrap();
    let id = send_list_request(&mut wallet, &shop, shop.app_id(), Some(version)).await;
    match standing {
        RequestStanding::Blocked => shop.app_mut().deny(&sender).await.unwrap(),
        RequestStanding::Flagged => lists.flag(&sender),
        RequestStanding::Unknown => lists.lists_unknown(true),
    }
    let received = shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app().protocol_scan("paykit", b"lrec/").await.unwrap(),
        before
    );
    assert_eq!(published(&shop, &wallet).await, sent);
    let unknown = matches!(standing, RequestStanding::Unknown);
    assert_eq!(received.applied.contains(&id), !unknown);
    assert_eq!(shop.spam_count(&sender).await.unwrap(), u64::from(!unknown));
    let marks = shop.app().protocol_scan("paykit", b"bd/").await.unwrap();
    assert_eq!(
        marks.len(),
        usize::from(matches!(standing, RequestStanding::Blocked))
    );
    if unknown {
        lists.lists_unknown(false);
        assert!(shop.receive(None).await.unwrap().applied.contains(&id));
        let issues = published(&shop, &wallet).await;
        assert_eq!(issues.len(), sent.len() + 1);
        assert_eq!(issues.last().unwrap().withdrawn, restored);
    }
}

#[tokio::test]
async fn test_blocked_private_list_request_only_marks_the_dropped_message() {
    for restored in [false, true] {
        check_request_intake(RequestStanding::Blocked, restored).await;
    }
}

#[tokio::test]
async fn test_flagged_private_list_request_only_counts_the_sender() {
    for restored in [false, true] {
        check_request_intake(RequestStanding::Flagged, restored).await;
    }
}

#[tokio::test]
async fn test_private_list_request_waits_for_the_senders_standing() {
    for restored in [false, true] {
        check_request_intake(RequestStanding::Unknown, restored).await;
    }
}

#[tokio::test]
async fn test_publication_answers_only_its_chosen_contact_and_resends_the_same_list_after_reload() {
    use crate::common::store;
    use paykit_sdk::{AppScopes, PaykitConfig};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    shop.app_mut().invite(&wallet.identity()).await.unwrap();
    let publisher = shop.app_id();
    let other = wallet.app_id();
    shop.set_list_endpoints(offered("lno1current"))
        .await
        .unwrap();
    request_list(&mut wallet, &mut shop, publisher, None).await;
    assert!(published(&shop, &wallet).await.is_empty());
    assert!(shop
        .app()
        .records_of("private_list_publications")
        .await
        .unwrap()
        .is_empty());
    shop.publish_list_to(wallet.identity()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let result = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await;
    assert!(matches!(result.outcome.unwrap(), PayOutcome::Paid { .. }));
    let first = published(&shop, &wallet).await;
    assert_eq!(first.len(), 1);
    request_list(&mut wallet, &mut shop, other, Some(first[0].version)).await;
    assert_eq!(published(&shop, &wallet).await, first);
    let day = 86_400_000;
    let now = shop.app().clock().now_ms();
    let evening = (now / day + 1) * day - 2 * 3_600_000;
    w.clock.advance_ms(if evening >= now {
        evening - now
    } else {
        evening + day - now
    });
    request_list(&mut wallet, &mut shop, publisher, Some(first[0].version)).await;
    let first_answer_at = shop.app().clock().now_ms();
    let initial = published(&shop, &wallet).await;
    assert_eq!(initial.len(), 2);
    assert_eq!(initial[1].version, first[0].version.next().unwrap());

    drop(shop);
    let mut shop = Paykit::load(
        w.env(&w.bob, storage),
        PaykitConfig::default(),
        publisher,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    w.clock.advance_ms(1_000);
    for (elapsed, count) in [(0, 1), (0, 1), (3_600_000, 2)] {
        w.clock.advance_ms(elapsed);
        request_list(&mut wallet, &mut shop, publisher, Some(first[0].version)).await;
        let mut expected = initial.clone();
        expected[1].repeats = count;
        assert_eq!(published(&shop, &wallet).await, expected);
    }
    wallet.receive(None).await.unwrap();
    let result = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.002")))
        .await;
    assert!(matches!(result.outcome.unwrap(), PayOutcome::Paid { .. }));

    let proofs = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    let mut paid = Vec::new();
    for signed in proofs
        .iter()
        .filter(|sent| sent.kind == paykit_lib::Kind::PaymentProof.as_str())
    {
        let message = pubky_switch_core::message::Message::parse(
            signed.plaintext.json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )
        .unwrap();
        let paykit_lib::Body::PaymentProof(proof) = paykit_lib::Body::parse(&message).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(proof.payment_app_id, publisher);
        paid.push(proof.list_version.unwrap());
    }
    paid.sort();
    assert_eq!(paid, [first[0].version, initial[1].version]);

    for count in 3..=5 {
        let held = published(&shop, &wallet).await.last().unwrap().version;
        if count == 3 {
            request_list(&mut wallet, &mut shop, None, None).await;
        } else {
            request_list(&mut wallet, &mut shop, publisher, Some(held)).await;
        }
        assert_eq!(published(&shop, &wallet).await.len(), count);
    }
    let held = published(&shop, &wallet).await;
    request_list(
        &mut wallet,
        &mut shop,
        publisher,
        Some(held.last().unwrap().version),
    )
    .await;
    assert_eq!(published(&shop, &wallet).await, held);
    let midnight = (first_answer_at / day + 1) * day;
    w.clock.advance_ms(midnight - shop.app().clock().now_ms());
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &wallet).await, held);
    w.clock
        .advance_ms(first_answer_at + day - shop.app().clock().now_ms());
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &wallet).await.len(), 6);
    request_list(&mut wallet, &mut shop, publisher, None).await;
    assert_eq!(published(&shop, &wallet).await.len(), 6);
    shop.set_list_endpoints(offered("lno1replacement"))
        .await
        .unwrap();
    let changed = published(&shop, &wallet).await;
    assert_eq!(changed.len(), 7);
    w.clock.advance_ms(2 * 3_600_000);
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &wallet).await, changed);
    shop.withdraw_list_from(wallet.identity()).await.unwrap();
    let withdrawal = published(&shop, &wallet).await;
    assert!(withdrawal.last().unwrap().withdrawn);
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.003")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::NoDestination(_))
        ),
        "{refused}"
    );
    assert_eq!(w.rail.all_executions().len(), 2);
    request_list(&mut wallet, &mut shop, publisher, Some(first[0].version)).await;
    assert_eq!(published(&shop, &wallet).await, withdrawal);
}

#[tokio::test]
async fn test_publication_request_for_an_unchanged_offer_uses_the_active_generation() {
    use crate::common::BOB_ORIGIN;
    use paykit_lib::ListVersion;
    use pubky_switch_core::records::Inbox;

    for moved in [false, true] {
        for names_offer in [true, false] {
            let w = World::new();
            let mut wallet = w.wallet("wallet").await;
            let mut shop = w.shop().await;
            let app = shop.app_id();
            shop.app_mut().invite(&wallet.identity()).await.unwrap();
            shop.set_list_endpoints(offered("lno1current"))
                .await
                .unwrap();
            shop.publish_list_to(wallet.identity()).await.unwrap();
            let first = ListVersion::new(1, 1).unwrap();
            assert_eq!(published(&shop, &wallet).await[0].version, first);

            if moved {
                let service = w.bob.switch().change_key_into([74; 32]);
                w.router.add_switch(BOB_ORIGIN, &service);
                let current = w
                    .bob
                    .sign_next(|body| {
                        body.generation = 2;
                        body.inbox = Inbox::new(BOB_ORIGIN, service.service_key());
                    })
                    .unwrap();
                service.deliver_identity(&current).unwrap();
                shop.app_mut().refresh_identity().await.unwrap();
                shop.app_mut().follow_provider().await.unwrap();
            }

            request_list(&mut wallet, &mut shop, app, names_offer.then_some(first)).await;
            let expected = if moved { (2, 1) } else { (1, 2) };
            let next = ListVersion::new(expected.0, expected.1).unwrap();
            let sent = published(&shop, &wallet).await;
            assert_eq!(
                sent.iter().map(|issue| issue.version).collect::<Vec<_>>(),
                [first, next]
            );
            assert!(!sent[1].withdrawn);
            shop.receive(None).await.unwrap();
            assert_eq!(published(&shop, &wallet).await, sent);
        }
    }
}

/// Two installations of the same publisher separately choose the payer. The second can
/// advance the shared counter without giving its signed list to the first installation.
async fn contemporaneous_publishers(
    w: &World,
) -> (
    Paykit,
    Paykit,
    Paykit,
    std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) {
    use crate::common::store;
    use paykit_sdk::{AppScopes, PaykitConfig};

    let wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    shop.app_mut().invite(&wallet.identity()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let mut second = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    second.receive(None).await.unwrap();
    for (publisher, offer) in [(&mut shop, "lno1first"), (&mut second, "lno1second")] {
        publisher.set_list_endpoints(offered(offer)).await.unwrap();
        publisher.publish_list_to(wallet.identity()).await.unwrap();
    }
    assert_eq!(shop.app_id(), second.app_id());
    assert_eq!(published(&shop, &wallet).await[0].version.counter(), 1);
    assert_eq!(published(&second, &wallet).await[0].version.counter(), 2);
    (wallet, shop, second, storage)
}

#[tokio::test]
async fn test_publication_retains_a_named_request_after_another_installation_publishes() {
    use paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS;
    use paykit_sdk::PaykitConfig;
    let w = World::new();
    let (mut wallet, mut shop, second, storage) = contemporaneous_publishers(&w).await;
    let publisher = shop.app_id();
    let first = published(&shop, &wallet).await;
    let winner = published(&second, &wallet).await;
    let key = shop
        .app()
        .record_key(&target(&shop, &wallet.identity(), 1))
        .unwrap();
    request_list(&mut wallet, &mut shop, publisher, Some(first[0].version)).await;
    assert_eq!(published(&shop, &wallet).await, first);
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    drop(shop);
    let mut shop = Paykit::load(
        w.env(&w.bob, storage),
        PaykitConfig::default(),
        publisher,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    w.clock.advance_ms(2 * PRIVATE_LIST_MAX_AGE_MS - 1);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    assert_eq!(
        w.record_writes(&w.bob, &key),
        3,
        "the retained named request became eligible"
    );
    let newest = published(&shop, &wallet).await.pop().unwrap();
    assert_eq!(newest.version, winner[0].version.next().unwrap());
    assert!(!newest.withdrawn);
    // The recipient's signed append time distinguishes a repeat from its first acceptance.
    w.clock.advance_ms(1);
    request_list(&mut wallet, &mut shop, publisher, Some(first[0].version)).await;
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    let repeated = published(&shop, &wallet).await.pop().unwrap();
    assert_eq!(
        (repeated.id, repeated.hash, repeated.repeats),
        (newest.id, newest.hash, 1)
    );
}

#[tokio::test]
async fn test_publication_missing_answers_age_once_across_reload_and_withdrawal() {
    use paykit_sdk::PaykitConfig;
    const DAY: u64 = 24 * 3_600_000;
    for withdraw in [false, true] {
        let w = World::new();
        let (mut wallet, mut shop, mut second, storage) = contemporaneous_publishers(&w).await;
        let publisher = shop.app_id();
        let recipient = wallet.identity();
        let key = shop
            .app()
            .record_key(&target(&shop, &recipient, 1))
            .unwrap();
        for offer in ["lno1third", "lno1fourth", "lno1fifth"] {
            second.set_list_endpoints(offered(offer)).await.unwrap();
        }
        assert_eq!(w.record_writes(&w.bob, &key), 5);
        request_list(&mut wallet, &mut shop, publisher, None).await;
        assert_eq!(w.record_writes(&w.bob, &key), 5);
        drop(shop);
        let mut shop = Paykit::load(
            w.env(&w.bob, storage),
            PaykitConfig::default(),
            publisher,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        w.clock.advance_ms(DAY - 1);
        shop.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.bob, &key), 5);
        if withdraw {
            shop.withdraw_list_from(recipient).await.unwrap();
        }
        w.clock.advance_ms(1);
        shop.receive(None).await.unwrap();
        assert_eq!(
            w.record_writes(&w.bob, &key),
            6,
            "the original daily window elapsed"
        );
        let newest = published(&shop, &wallet).await.pop().unwrap();
        assert_eq!(newest.version.counter(), 6);
        assert_eq!(newest.withdrawn, withdraw);
        if withdraw {
            request_list(&mut wallet, &mut shop, publisher, None).await;
            assert_eq!(w.record_writes(&w.bob, &key), 6);
        }
    }
}

#[tokio::test]
async fn test_publication_repeats_the_newest_signed_list_after_a_generation_change() {
    for second_installation_withdraws in [false, true] {
        repeat_across_generations(second_installation_withdraws).await;
    }
}

async fn repeat_across_generations(second_installation_withdraws: bool) {
    use crate::common::{store, BOB_ORIGIN};
    use paykit_sdk::PaykitConfig;
    use pubky_switch_core::records::Inbox;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let publisher = shop.app_id();
    let recipient = wallet.identity();
    shop.app_mut().invite(&recipient).await.unwrap();
    shop.set_list_endpoints(offered("lno1current"))
        .await
        .unwrap();
    shop.publish_list_to(recipient).await.unwrap();
    let first = published(&shop, &wallet).await[0].version;
    request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
    let original = published(&shop, &wallet).await;
    assert_eq!(original.len(), 2);
    assert_eq!(original[1].version, first.next().unwrap());

    let backup = shop.backup().await.unwrap();
    let service = w.bob.switch().change_key_into([74; 32]);
    w.router.add_switch(BOB_ORIGIN, &service);
    let current = w
        .bob
        .sign_next(|body| {
            body.generation = 2;
            body.inbox = Inbox::new(BOB_ORIGIN, service.service_key());
        })
        .unwrap();
    service.deliver_identity(&current).unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    w.clock.advance_ms(3_600_000);
    request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
    let mut expected = original;
    expected[1].repeats += 1;
    assert_eq!(published(&shop, &wallet).await, expected);
    let current_key = target(&shop, &recipient, 2);
    assert!(matches!(
        shop.app_mut()
            .read_record_fresh(&current_key)
            .await
            .unwrap(),
        RecordRead::Kept {
            record: None,
            reason: KeptReason::NotFound
        }
    ));

    if second_installation_withdraws {
        // A second installation of the same publisher withdraws this recipient's list.
        let mut restored = Paykit::restore(
            w.env(&w.bob, store()),
            PaykitConfig::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        restored.receive(None).await.unwrap();
        let RecordRead::Fresh(record) = restored
            .app_mut()
            .read_record_fresh(&current_key)
            .await
            .unwrap()
        else {
            panic!("the restored publisher did not reserve its withdrawal")
        };
        let withdrawal = ListPublicationRecord::read(record.content.value().unwrap()).unwrap();
        assert_eq!((withdrawal.generation, withdrawal.counter), (2, 1));
        assert!(published(&restored, &wallet)
            .await
            .iter()
            .any(|sent| sent.withdrawn && sent.id == withdrawal.msg_id));
        w.clock.advance_ms(3_600_000);
        request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
        assert_eq!(published(&shop, &wallet).await, expected);
    }
    let last = service.change_key_into([75; 32]);
    w.router.add_switch(BOB_ORIGIN, &last);
    let moved = w
        .bob
        .sign_next(|body| {
            body.generation = 3;
            body.inbox = Inbox::new(BOB_ORIGIN, last.service_key());
        })
        .unwrap();
    last.deliver_identity(&moved).unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    w.clock.advance_ms(3_600_000);
    request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
    // A missing current inventory asks the next receive to list the collection.
    shop.receive(None).await.unwrap();
    if !second_installation_withdraws {
        expected[1].repeats += 1;
    }
    assert_eq!(published(&shop, &wallet).await, expected);
    let latest = target(&shop, &recipient, 3);
    assert!(matches!(
        shop.app_mut().read_record_fresh(&latest).await.unwrap(),
        RecordRead::Kept {
            record: None,
            reason: KeptReason::NotFound
        }
    ));
}

#[tokio::test]
async fn test_publication_waits_for_an_unknown_counter_key_before_repeating_an_older_list() {
    use crate::common::{ALICE_ORIGIN, HOUR_MS};
    use crate::unrequested::store_bundle_at;
    use paykit_sdk::AppScopes;
    use pubky_switch_client::app::{RecordContent, Unopened};
    use pubky_switch_client::testing::ScriptedSwitch;
    use pubky_switch_core::records::Inbox;

    let w = World::new();
    let scopes = AppScopes::new().request().build().unwrap();
    let mut shop = w.join(&w.alice, "shop", scopes.clone()).await;
    let mut other = w.join(&w.alice, "other shop", scopes).await;
    let mut wallet = w
        .join(&w.bob, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let recipient = wallet.identity();
    let publisher = shop.app_id();
    shop.app_mut().invite(&recipient).await.unwrap();
    wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    shop.publish_list_to(recipient).await.unwrap();
    let first = published(&shop, &wallet).await[0].version;
    request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
    let mut expected = published(&shop, &wallet).await;
    assert_eq!(expected.len(), 2);

    let move_to = |previous: &ScriptedSwitch, generation, secret| {
        let service = previous.change_key_into(secret);
        w.router.add_switch(ALICE_ORIGIN, &service);
        let doc = w
            .alice
            .sign_next(|body| {
                body.generation = generation;
                body.inbox = Inbox::new(ALICE_ORIGIN, service.service_key());
            })
            .unwrap();
        service.deliver_identity(&doc).unwrap();
        service
    };
    let rotate = |service: &ScriptedSwitch, epoch| {
        let doc = w.alice.sign_next(|body| body.data_epoch = epoch).unwrap();
        service.deliver_identity(&doc).unwrap();
    };
    let middle = move_to(w.alice.switch(), 2, [74; 32]);
    for seller in [&mut shop, &mut other] {
        seller.app_mut().refresh_identity().await.unwrap();
        seller.app_mut().follow_provider().await.unwrap();
    }
    rotate(&middle, 2);
    store_bundle_at(&w, &middle, other.app_id(), other.app().cert(), 1);
    shop.app_mut().refresh_identity().await.unwrap();
    other.receive(None).await.unwrap();
    other.receive(None).await.unwrap();
    other.app_mut().invite(&recipient).await.unwrap();
    other
        .set_list_endpoints(offered("lno1other"))
        .await
        .unwrap();
    other.publish_list_to(recipient).await.unwrap();
    let opaque = other
        .app()
        .record_key(&target(&other, &recipient, 2))
        .unwrap();
    let issued = other.app().shared_record(&opaque).await.unwrap().unwrap();
    let counter = ListPublicationRecord::read(issued.content.value().unwrap()).unwrap();
    assert_eq!(
        (counter.publisher, counter.generation, counter.counter),
        (other.app_id(), 2, 1)
    );

    let latest = move_to(&middle, 3, [75; 32]);
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    rotate(&latest, 3);
    store_bundle_at(&w, &latest, publisher, shop.app().cert(), 3);
    let family = Collection::with_permanent_generations(
        "private_list_publications",
        "paykit_request",
        &["paykit_request"],
    );
    let mut observed = None;
    for _ in 0..3 {
        let read = shop.receive(None).await.unwrap();
        observed = shop
            .app_mut()
            .current_collection(&family, &read.report)
            .await
            .unwrap();
        if observed.is_some() {
            break;
        }
    }
    assert!(observed.unwrap().uncertain_keys.contains(&opaque));
    let held = shop.app().shared_record(&opaque).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        RecordContent::Unreadable(Unopened::KeyNotHeld { data_epoch: 2 })
    );
    assert!(shop.app().scope_key(3, "paykit_request").is_some());
    w.clock.advance_ms(HOUR_MS);
    request_list(&mut wallet, &mut shop, publisher, Some(first)).await;
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &wallet).await, expected);

    // The missing key identifies the counter as another app's publication.
    store_bundle_at(&w, &latest, publisher, shop.app().cert(), 1);
    for _ in 0..3 {
        shop.receive(None).await.unwrap();
    }
    expected[1].repeats += 1;
    assert_eq!(published(&shop, &wallet).await, expected);
}

#[tokio::test]
async fn test_publication_restored_answers_use_the_retained_request_and_current_sharing_choice() {
    use crate::common::{store, BOB_ORIGIN};
    use paykit_sdk::{AppScopes, PaykitConfig};
    use pubky_switch_core::records::Inbox;

    for (in_place, moved) in [(false, false), (true, false), (false, true), (true, true)] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let storage = store();
        let mut shop = w
            .join_with(
                &w.bob,
                "publisher",
                AppScopes::new().request().build().unwrap(),
                PaykitConfig::default(),
                storage.clone(),
            )
            .await;
        let app = shop.app_id();
        shop.app_mut().invite(&wallet.identity()).await.unwrap();
        shop.set_list_endpoints(offered("lno1old")).await.unwrap();
        shop.publish_list_to(wallet.identity()).await.unwrap();
        let old = published(&shop, &wallet).await[0].version;
        w.clock.advance_ms(31 * 86_400_000);
        shop.set_list_endpoints(offered("lno1current"))
            .await
            .unwrap();
        let before = shop.backup().await.unwrap();
        request_list(&mut wallet, &mut shop, app, Some(old)).await;
        let answered = published(&shop, &wallet).await.last().unwrap().version;
        assert_eq!(answered.counter(), 3);
        if moved {
            let changed = w.bob.switch().change_key_into([74; 32]);
            w.router.add_switch(BOB_ORIGIN, &changed);
            let current = w
                .bob
                .sign_next(|body| {
                    body.generation = 2;
                    body.inbox = Inbox::new(BOB_ORIGIN, changed.service_key());
                })
                .unwrap();
            changed.deliver_identity(&current).unwrap();
        }
        drop(shop);

        let destination = if in_place { storage } else { store() };
        let mut shop = Paykit::restore(
            w.env(&w.bob, destination),
            PaykitConfig::default(),
            &before,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        if moved {
            shop.app_mut().refresh_identity().await.unwrap();
            shop.app_mut().follow_provider().await.unwrap();
        }
        shop.receive(None).await.unwrap();
        let withdrawals = published(&shop, &wallet).await;
        assert!(withdrawals.last().unwrap().withdrawn);
        request_list(&mut wallet, &mut shop, app, Some(old)).await;
        assert_eq!(published(&shop, &wallet).await, withdrawals);
        shop.set_list_endpoints(offered("lno1chosen"))
            .await
            .unwrap();
        shop.publish_list_to(wallet.identity()).await.unwrap();
        let fresh = published(&shop, &wallet).await;
        assert_eq!(
            fresh.last().unwrap().version,
            if moved {
                paykit_lib::ListVersion::new(2, 2).unwrap()
            } else {
                paykit_lib::ListVersion::new(1, 5).unwrap()
            }
        );
        request_list(&mut wallet, &mut shop, app, Some(old)).await;
        assert_eq!(published(&shop, &wallet).await, fresh);
        w.clock.advance_ms(3_600_000);
        request_list(&mut wallet, &mut shop, app, Some(old)).await;
        let mut expected = fresh;
        if in_place {
            expected.last_mut().unwrap().repeats += 1;
        }
        assert_eq!(
            published(&shop, &wallet).await,
            expected,
            "in-place={in_place}, moved={moved}"
        );
    }
}

#[tokio::test]
async fn test_publication_replaces_an_undelivered_withdrawal_only_when_due() {
    use crate::common::{store, HOUR_MS};
    use paykit_sdk::{AppScopes, PaykitConfig};
    use pubky_switch_client::app::{AppEvent, Delivery};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    let publisher = shop.app_id();
    let payer = w.alice_id();
    shop.app_mut().invite(&payer).await.unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    shop.publish_list_to(payer).await.unwrap();
    wallet.receive(None).await.unwrap();
    let pay = || UnrequestedPayment::new(w.bob_id(), btc("0.001"));
    assert!(matches!(
        wallet.pay_without_request(pay()).await.outcome.unwrap(),
        PayOutcome::Paid { .. }
    ));
    let at = target(&shop, &payer, shop.app().own_identity().generation());
    let key = shop.app().record_key(&at).unwrap();
    shop.app_mut().hold_queued(&payer).await.unwrap();
    shop.withdraw_list_from(payer).await.unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let empty = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(empty.counter, 2);
    assert!(matches!(
        shop.app().delivery(&empty.msg_id).await.unwrap(),
        Some(Delivery::Queued { .. })
    ));
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &empty.msg_id)
        .await
        .unwrap());
    shop.app_mut().release_queued(&payer).await.unwrap();
    let report = shop.receive(None).await.unwrap();
    assert!(report.report.events.iter().any(|event| matches!(event,
        AppEvent::MessageUndelivered { msg_id, resend: false, .. } if msg_id == &empty.msg_id)));
    drop(shop);
    let mut shop = Paykit::load(
        w.env(&w.bob, storage),
        PaykitConfig::default(),
        publisher,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    w.clock.advance_ms(HOUR_MS - 1);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let replacement = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(
        replacement.counter, 3,
        "a proven undelivered withdrawal needs a new reservation"
    );
    assert_ne!(replacement.msg_id, empty.msg_id);
    assert!(matches!(
        shop.app().delivery(&replacement.msg_id).await.unwrap(),
        Some(Delivery::Accepted { .. })
    ));
    wallet.receive(None).await.unwrap();
    assert!(matches!(
        wallet
            .pay_without_request(pay())
            .await
            .outcome
            .as_ref()
            .err()
            .and_then(|e| e.refusal()),
        Some(paykit_sdk::Refusal::NoDestination(_))
    ));
    w.clock.advance_ms(24 * HOUR_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
}

async fn give_up_withdrawal(shop: &mut Paykit, payer: [u8; 32]) -> ListPublicationRecord {
    shop.app_mut().hold_queued(&payer).await.unwrap();
    shop.withdraw_list_from(payer).await.unwrap();
    let target = target(shop, &payer, shop.app().own_identity().generation());
    let key = shop.app().record_key(&target).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let issue = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &issue.msg_id)
        .await
        .unwrap());
    shop.app_mut().release_queued(&payer).await.unwrap();
    issue
}

#[tokio::test]
async fn test_publication_reselecting_one_contact_retires_only_its_withdrawal_retry() {
    use crate::common::HOUR_MS;
    use paykit_lib::ListVersion;
    use paykit_sdk::AppScopes;

    let w = World::new();
    let alice = w.wallet("alice").await;
    let mut carol = w
        .join(&w.carol, "carol", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    carol.app_mut().invite(&shop.identity()).await.unwrap();
    shop.set_list_endpoints(offered("lno1current"))
        .await
        .unwrap();
    for payer in [&alice, &carol] {
        let id = payer.identity();
        shop.app_mut().invite(&id).await.unwrap();
        shop.publish_list_to(id).await.unwrap();
        assert_eq!(published(&shop, payer).await[0].version.counter(), 1);
        assert_eq!(give_up_withdrawal(&mut shop, id).await.counter, 2);
    }
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS / 2);
    shop.publish_list_to(alice.identity()).await.unwrap();
    let reselected = published(&shop, &alice).await;
    assert_eq!(reselected.last().unwrap().version.counter(), 3);
    assert!(!reselected.last().unwrap().withdrawn);
    shop.app_mut()
        .remove_conversation(&carol.identity())
        .await
        .unwrap();
    let before = shop.app_mut().allow_list().await.unwrap();
    assert!(!before.contacts.contains(&carol.identity()));
    assert!(!before
        .conversations
        .iter()
        .any(|c| c.pubky == carol.identity()));
    let before_retry = published(&shop, &carol).await;
    assert_eq!(before_retry.len(), 1);
    assert_eq!(before_retry[0].version, ListVersion::new(1, 1).unwrap());
    assert!(!before_retry[0].withdrawn);
    w.clock.advance_ms(HOUR_MS / 2);
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &alice).await, reselected);
    let left = published(&shop, &carol).await;
    assert_eq!(left.len(), 2);
    assert_eq!(left[0], before_retry[0]);
    let withdrawal = &left[1];
    assert_eq!(withdrawal.version, ListVersion::new(1, 3).unwrap());
    assert!(withdrawal.withdrawn);
    assert_eq!(withdrawal.repeats, 0);
    assert!(matches!(
        shop.app().delivery(&withdrawal.id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    assert_eq!(shop.app_mut().allow_list().await.unwrap(), before);

    // A second withdrawal starts from its own issue, even after the old interval elapsed.
    let second = give_up_withdrawal(&mut shop, alice.identity()).await;
    assert_eq!(second.counter, 4);
    w.clock.advance_ms(HOUR_MS - 1);
    shop.receive(None).await.unwrap();
    assert_eq!(published(&shop, &alice).await, reselected);
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    let latest = published(&shop, &alice).await;
    assert_eq!(latest.last().unwrap().version.counter(), 5);
    assert!(latest.last().unwrap().withdrawn);
    assert_eq!(published(&shop, &carol).await, left);

    let before_resend = shop.app_mut().allow_list().await.unwrap();
    assert_eq!(before_resend, before);
    assert!(matches!(
        shop.app_mut().resend(&withdrawal.id).await.unwrap(),
        pubky_switch_client::app::Delivery::Accepted { .. }
    ));
    let resent = published(&shop, &carol).await;
    assert_eq!(
        resent.last().unwrap(),
        &Published {
            repeats: 1,
            ..withdrawal.clone()
        }
    );
    assert_eq!(shop.app_mut().allow_list().await.unwrap(), before_resend);
}

#[tokio::test]
async fn test_publication_retry_waits_when_a_restored_publisher_already_withdrew_a_newer_list() {
    use crate::common::{store, HOUR_MS};
    use paykit_sdk::PaykitConfig;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let payer = buyer.identity();
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1original"))
        .await
        .unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let empty = give_up_withdrawal(&mut shop, payer).await;
    shop.receive(None).await.unwrap();
    let saved = shop.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &saved,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    let key = shop
        .app()
        .record_key(&target(&shop, &payer, empty.generation))
        .unwrap();
    let issued = published(&restored, &buyer).await;
    assert_eq!(issued.last().unwrap().version.counter(), empty.counter + 1);
    assert!(issued.last().unwrap().withdrawn);
    assert_ne!(issued.last().unwrap().id, empty.msg_id);
    assert_eq!(w.record_writes(&w.bob, &key), 3);

    w.clock.advance_ms(HOUR_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    assert_eq!(
        published(&shop, &buyer)
            .await
            .last()
            .unwrap()
            .version
            .counter(),
        1
    );
}

#[tokio::test]
async fn test_publication_retry_after_a_move_requires_the_original_application() {
    use crate::common::{BOB_ORIGIN, HOUR_MS};
    use pubky_switch_core::ops::{Condition, OpKind};
    use pubky_switch_core::records::Inbox;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let payer = buyer.identity();
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let original = give_up_withdrawal(&mut shop, payer).await;
    shop.receive(None).await.unwrap();

    let next = w.bob.switch().change_key_into([74; 32]);
    w.router.add_switch(BOB_ORIGIN, &next);
    let moved = w
        .bob
        .sign_next(|doc| {
            doc.generation = 2;
            doc.inbox = Inbox::new(BOB_ORIGIN, next.service_key());
        })
        .unwrap();
    next.deliver_identity(&moved).unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().follow_provider().await.unwrap();
    let old = target(&shop, &payer, original.generation);
    let RecordApplication {
        record: held,
        application,
        ..
    } = shop
        .app_mut()
        .read_record_application(&old)
        .await
        .unwrap()
        .unwrap();
    let required = Condition::RecordCurrent {
        key: held.key,
        revision: application,
        deleted: false,
    };
    w.clock.advance_ms(HOUR_MS);
    shop.receive(None).await.unwrap();
    let lists = published(&shop, &buyer).await;
    let sent = lists.last().unwrap();
    assert_eq!(sent.version, paykit_lib::ListVersion::new(2, 1).unwrap());
    assert!(sent.withdrawn);
    assert_ne!(sent.id, original.msg_id);
    let new_key = shop.app().record_key(&target(&shop, &payer, 2)).unwrap();
    assert!(next.applied().iter().any(|op| {
        matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == new_key)
            && op.body().conditions.contains(&required)
    }));
}

#[tokio::test]
async fn test_publication_withdrawal_retry_respects_a_newer_issue_in_an_intervening_generation() {
    for restored_withdraws in [false, true] {
        retry_after_two_moves(restored_withdraws).await;
    }
}

async fn retry_after_two_moves(restored_withdraws: bool) {
    use crate::common::{store, BOB_ORIGIN, HOUR_MS};
    use paykit_sdk::PaykitConfig;
    use pubky_switch_core::records::Inbox;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let payer = buyer.identity();
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    assert_eq!(give_up_withdrawal(&mut shop, payer).await.counter, 2);
    shop.receive(None).await.unwrap();
    let saved = shop.backup().await.unwrap();

    let mut service = w.bob.switch().clone();
    for generation in 2..=3 {
        let next = service.change_key_into([70 + generation as u8; 32]);
        w.router.add_switch(BOB_ORIGIN, &next);
        let moved = w
            .bob
            .sign_next(|body| {
                body.generation = generation;
                body.inbox = Inbox::new(BOB_ORIGIN, next.service_key());
            })
            .unwrap();
        next.deliver_identity(&moved).unwrap();
        shop.app_mut().refresh_identity().await.unwrap();
        shop.app_mut().follow_provider().await.unwrap();
        if generation == 2 && restored_withdraws {
            let mut restored = Paykit::restore(
                w.env(&w.bob, store()),
                PaykitConfig::default(),
                &saved,
                w.rail.clone(),
                w.timer(),
            )
            .await
            .unwrap();
            restored.receive(None).await.unwrap();
            let issues = published(&restored, &buyer).await;
            let newer = issues.last().unwrap();
            assert_eq!(newer.version, paykit_lib::ListVersion::new(2, 1).unwrap());
            assert!(newer.withdrawn);
        }
        service = next;
    }
    w.clock.advance_ms(HOUR_MS);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let current = target(&shop, &payer, 3);
    let read = shop.app_mut().read_record_fresh(&current).await.unwrap();
    let delivered = published(&shop, &buyer).await;
    if restored_withdraws {
        assert!(matches!(
            read,
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound
            }
        ));
        assert_eq!(delivered.len(), 1);
    } else {
        let RecordRead::Fresh(held) = read else {
            panic!("missing withdrawal after two moves: {read:?}")
        };
        let issue = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
        assert_eq!((issue.generation, issue.counter), (3, 1));
        assert_eq!(delivered.len(), 2);
        assert!(delivered[1].withdrawn);
        assert_eq!(
            (&issue.msg_id, issue.message_hash),
            (&delivered[1].id, delivered[1].hash)
        );
    }
}

#[tokio::test]
async fn test_publication_stops_retrying_when_its_only_older_offer_never_reached_the_payer() {
    for receive_before_withdrawal in [true, false] {
        let w = World::new();
        let buyer = w.wallet("wallet").await;
        let payer = buyer.identity();
        let mut shop = w.shop().await;
        shop.app_mut().invite(&payer).await.unwrap();
        shop.set_list_endpoints(offered("lno1never")).await.unwrap();
        shop.app_mut().hold_queued(&payer).await.unwrap();
        shop.publish_list_to(payer).await.unwrap();
        let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
        let held = shop.app().shared_record(&key).await.unwrap().unwrap();
        let first = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
        assert!(shop
            .app_mut()
            .withdraw_message("paykit", &first.msg_id)
            .await
            .unwrap());
        if receive_before_withdrawal {
            shop.receive(None).await.unwrap();
        }
        assert_eq!(give_up_withdrawal(&mut shop, payer).await.counter, 2);
        w.clock.advance_ms(crate::common::HOUR_MS);
        shop.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.bob, &key), 2);
        assert!(published(&shop, &buyer).await.is_empty());
    }
}

#[tokio::test]
async fn test_publication_keeps_an_unknown_withdrawal_and_a_failed_resend_from_authorizing_another()
{
    use pubky_switch_client::app::AppEvent;
    use pubky_switch_client::testing::Fault;

    for accepted in [false, true] {
        let w = World::new();
        let mut buyer = w.wallet("wallet").await;
        let payer = buyer.identity();
        let mut shop = w.shop().await;
        shop.app_mut().invite(&payer).await.unwrap();
        shop.set_list_endpoints(offered("lno1first")).await.unwrap();
        shop.publish_list_to(payer).await.unwrap();
        if !accepted {
            w.alice.switch().fail_next(Fault::LoseAnswer {
                path: "inbox".into(),
            });
        }
        shop.withdraw_list_from(payer).await.unwrap();
        let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
        let held = shop.app().shared_record(&key).await.unwrap().unwrap();
        let empty = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
        if accepted {
            shop.app_mut().hold_queued(&payer).await.unwrap();
            shop.app_mut().resend(&empty.msg_id).await.unwrap();
        }
        assert!(shop
            .app_mut()
            .withdraw_message("paykit", &empty.msg_id)
            .await
            .unwrap());
        let seen = shop.receive(None).await.unwrap();
        assert!(seen.report.events.iter().any(|event| match event {
            AppEvent::DeliveryUnknown { msg_id, .. } => !accepted && msg_id == &empty.msg_id,
            AppEvent::MessageUndelivered {
                msg_id,
                resend: true,
                ..
            } => accepted && msg_id == &empty.msg_id,
            _ => false,
        }));
        shop.app_mut().release_queued(&payer).await.unwrap();
        w.clock.advance_ms(2 * crate::common::HOUR_MS);
        shop.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.bob, &key), 2);
        buyer.receive(None).await.unwrap();
        let pay = buyer
            .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
            .await;
        assert!(matches!(
            pay.outcome.as_ref().err().and_then(|e| e.refusal()),
            Some(paykit_sdk::Refusal::NoDestination(_))
        ));
    }
}

/// The shop's current list to `payer`, which `send` reserves while the payer's inbox
/// takes nothing, then the client gives up: its delivery is unknown. Returns the
/// publication and its record key.
async fn unknown_list(
    w: &World,
    shop: &mut Paykit,
    payer: [u8; 32],
    send: impl AsyncFnOnce(&mut Paykit),
) -> (ListPublicationRecord, [u8; 32]) {
    use pubky_switch_client::testing::Fault;
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "inbox".into(),
        after: 0,
    });
    send(shop).await;
    let key = shop.app().record_key(&target(shop, &payer, 1)).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let issue = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &issue.msg_id)
        .await
        .unwrap());
    (issue, key)
}

/// The list notices `shop` keeps, acknowledged or not (`nt/outgoing/`).
async fn list_notices(shop: &Paykit) -> Vec<Vec<u8>> {
    shop.app()
        .protocol_scan("paykit", b"nt/outgoing/")
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

#[tokio::test]
async fn test_publication_acknowledged_list_notice_goes_once_its_list_expires() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (list, _) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let notice = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.msg_id.as_deref() == Some(list.msg_id.as_str()))
        .unwrap();
    // Acknowledged, it is kept while the list's outcome matters, so it is raised once.
    shop.acknowledge_notice(&notice.id).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(list_notices(&shop).await.len(), 1);
    // Once the list has expired, nothing it offered can be paid: the notice goes.
    w.clock
        .advance_ms(paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS + crate::common::HOUR_MS);
    shop.receive(None).await.unwrap();
    assert!(list_notices(&shop).await.is_empty());
    assert!(shop.notices().await.unwrap().is_empty());
    drop(buyer);
}

/// The value of the refusal reporting a list of unknown delivery.
fn unknown_shown(error: paykit_sdk::PaykitError) -> paykit_sdk::UnknownListDelivery {
    match error.refusal() {
        Some(paykit_sdk::Refusal::ListDeliveryUnknown(shown)) => (**shown).clone(),
        _ => panic!("the call must report the unknown delivery: {error:?}"),
    }
}

/// Whether `buyer` can pay the shop now without a request.
async fn pays(w: &World, buyer: &mut Paykit) -> bool {
    let pay = buyer
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.001")))
        .await;
    match pay.outcome {
        Ok(PayOutcome::Paid { .. }) => true,
        Err(error)
            if matches!(
                error.refusal(),
                Some(paykit_sdk::Refusal::NoDestination(_) | paykit_sdk::Refusal::WaitingForList)
            ) =>
        {
            false
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_publication_reports_an_unknown_withdrawal_and_sends_another_only_once_confirmed() {
    use paykit_sdk::{NoticeKind, Refusal, UserConfirmed};
    use pubky_switch_client::clock::Clock;

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let storage = crate::common::store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&storage),
        )
        .await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    buyer.receive(None).await.unwrap();
    let (empty, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let notices = shop.notices().await.unwrap();
    let notice = notices
        .iter()
        .find(|n| n.kind == NoticeKind::ListDeliveryUnknown)
        .unwrap();
    assert_eq!(notice.counterparty, Some(payer));
    assert_eq!(notice.msg_id.as_deref(), Some(empty.msg_id.as_str()));
    assert_eq!(notice.message_hash, Some(empty.message_hash));

    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    assert_eq!(
        (shown.recipient(), shown.version(), shown.withdrawal()),
        (payer, paykit_sdk::ListVersion::new(1, 2).unwrap(), true)
    );
    assert_eq!(shown.msg_id(), empty.msg_id);
    assert!(shown
        .payable_until_ms()
        .is_some_and(|end| end > w.clock.now_ms()));
    // The payer never got the withdrawal: it still pays the earlier list.
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);

    // No receive, retry interval or acknowledgement sends a replacement on its own.
    shop.acknowledge_notice(&notice.id).await.unwrap();
    w.clock.advance_ms(2 * crate::common::HOUR_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    // The outcome stays unknown across a restart.
    let app_id = shop.app_id();
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, storage, app_id, std::sync::Arc::clone(&w.clock))
        .await;
    shop.receive(None).await.unwrap();
    assert!(shop.notices().await.unwrap().is_empty());
    assert_eq!(
        unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(w.record_writes(&w.bob, &key), 2);

    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let again = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(again.counter, 3);
    // One confirmation sends one list.
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    shop.withdraw_list_from(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    buyer.receive(None).await.unwrap();
    assert!(!pays(&w, &mut buyer).await);
    assert!(!matches!(
        shop.withdraw_list_from(payer)
            .await
            .err()
            .and_then(|e| e.refusal().cloned()),
        Some(Refusal::ListDeliveryUnknown(_))
    ));
}

#[tokio::test]
async fn test_publication_refuses_an_earlier_unknown_delivery_shown_again() {
    use paykit_sdk::UserConfirmed;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let first = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    // The confirmed withdrawal's delivery ends unknown as well.
    unknown_list(&w, &mut shop, payer, async |shop| {
        shop.send_list_again(&first, UserConfirmed::new())
            .await
            .unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let second = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    assert_eq!(
        second.version(),
        paykit_sdk::ListVersion::new(1, 3).unwrap()
    );
    let writes = w.record_writes(&w.bob, &key);
    // The earlier value, kept by the host, confirms nothing now: the current one is shown.
    let refused = shop
        .send_list_again(&first, UserConfirmed::new())
        .await
        .unwrap_err();
    assert_eq!(unknown_shown(refused), second);
    assert_eq!(w.record_writes(&w.bob, &key), writes);
    drop(buyer);
}

#[tokio::test]
async fn test_publication_takes_a_counterparty_chosen_again_over_a_confirmed_withdrawal() {
    use paykit_sdk::UserConfirmed;

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    // Taken from the pending duty, before any receive.
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    shop.publish_list_to(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    // The list chosen again supersedes the unknown withdrawal: nothing more is sent.
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);
}

#[tokio::test]
async fn test_publication_reports_an_unknown_nonempty_list_and_sends_it_again_once_confirmed() {
    use paykit_sdk::{NoticeKind, UserConfirmed};

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (list, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    assert!(shop.notices().await.unwrap().iter().any(|n| {
        n.kind == NoticeKind::ListDeliveryUnknown
            && n.msg_id.as_deref() == Some(list.msg_id.as_str())
    }));
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    assert!(!shown.withdrawal());
    assert_eq!(shown.version(), paykit_sdk::ListVersion::new(1, 1).unwrap());
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    buyer.receive(None).await.unwrap();
    assert!(!pays(&w, &mut buyer).await);
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    shop.publish_list_to(payer).await.unwrap();
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);
}

#[tokio::test]
async fn test_publication_withdrawal_by_an_emptied_list_takes_the_same_confirmation() {
    use paykit_sdk::UserConfirmed;

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    buyer.receive(None).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.set_list_endpoints(std::collections::BTreeMap::new())
            .await
            .unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    // Withdrawing the counterparty sends nothing more on its own.
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    assert!(shown.withdrawal());
    assert_eq!(
        unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err()),
        shown
    );
    // Choosing it again with the list still empty would send the same empty content.
    assert_eq!(
        unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 3);
    buyer.receive(None).await.unwrap();
    assert!(!pays(&w, &mut buyer).await);
}

#[tokio::test]
async fn test_publication_keeps_a_confirmation_until_a_receive_sends_its_list() {
    use paykit_sdk::{AppScopes, PaykitConfig, Refusal, UserConfirmed};

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    // Sends to the counterparty are held: the confirmed list can't be reserved, before
    // any batch exists, and the confirmation is kept.
    shop.block(&payer).await.unwrap();
    let refused = shop
        .send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert_eq!(shop.lists_waiting_to_send_again().await.unwrap(), [payer]);
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    assert!(shop.lists_waiting_to_send_again().await.unwrap().is_empty());
    shop.publish_list_to(payer).await.unwrap();
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);
}

#[tokio::test]
async fn test_publication_change_undone_before_it_went_sends_nothing_over_an_unknown_list() {
    use paykit_sdk::{AppScopes, PaykitConfig};

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    // While sends are held a change is reserved for no one; undoing it leaves the same list.
    shop.block(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1second"))
        .await
        .unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert_eq!(
        unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
        shown
    );
}

/// Inject a list request from alice's wallet to `shop`, naming `held`.
fn ask_shop(w: &World, shop: &Paykit, held: Option<paykit_sdk::ListVersion>) {
    let body = paykit_lib::bodies::PrivatePaymentListRequest {
        held_version: held,
        app_id: Some(shop.app_id()),
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        paykit_lib::Kind::PrivatePaymentListRequest.as_str(),
        Vec::new(),
        body,
    );
}

#[tokio::test]
async fn test_publication_answers_past_their_bounds_issue_nothing_over_a_given_up_version() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
    // Four requests naming the newest version each get a newer one: the daily bound.
    for counter in 1..=4 {
        ask_shop(
            &w,
            &shop,
            Some(paykit_sdk::ListVersion::new(1, counter).unwrap()),
        );
        shop.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.bob, &key), counter as usize + 1);
    }
    let (_, _) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.set_list_endpoints(offered("lno1second"))
            .await
            .unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 6);
    // A request naming a version already answered gets the newest sent again, nothing newly
    // issued; that one was given up, so nothing goes.
    ask_shop(&w, &shop, Some(paykit_sdk::ListVersion::new(1, 5).unwrap()));
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 6);
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    assert_eq!(shown.version(), paykit_sdk::ListVersion::new(1, 6).unwrap());
}

#[tokio::test]
async fn test_publication_notice_of_a_version_superseded_while_its_batch_waited_goes() {
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "inbox".into(),
        after: 0,
    });
    shop.publish_list_to(payer).await.unwrap();
    let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let first = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    // The change's batch waits unanswered while the first version is given up.
    w.bob.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    let _ = shop.set_list_endpoints(offered("lno1second")).await;
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &first.msg_id)
        .await
        .unwrap());
    for _ in 0..3 {
        let _ = shop.receive(None).await;
    }
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    assert!(!shop
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.kind == paykit_sdk::NoticeKind::ListDeliveryUnknown));
}

#[tokio::test]
async fn test_publication_answers_a_list_request_over_a_list_of_unknown_delivery() {
    for expired in [false, true] {
        let w = World::new();
        let mut buyer = w.wallet("wallet").await;
        let payer = buyer.identity();
        let mut shop = w.shop().await;
        shop.app_mut().invite(&payer).await.unwrap();
        shop.set_list_endpoints(offered("lno1first")).await.unwrap();
        let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
            shop.publish_list_to(payer).await.unwrap();
        })
        .await;
        shop.receive(None).await.unwrap();
        if expired {
            w.clock
                .advance_ms(paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS + crate::common::HOUR_MS);
        }
        // The payer has no list: paying asks for one, and the answer is a newer version.
        buyer.receive(None).await.unwrap();
        assert!(!pays(&w, &mut buyer).await, "expired {expired}");
        shop.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.bob, &key), 2, "expired {expired}");
        // The answer superseded the list of unknown delivery: its notice is gone.
        assert!(!shop
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::ListDeliveryUnknown));
        shop.publish_list_to(payer).await.unwrap();
        buyer.receive(None).await.unwrap();
        assert!(pays(&w, &mut buyer).await, "expired {expired}");
    }
}

#[tokio::test]
async fn test_publication_confirmed_withdrawal_builds_on_a_counter_another_installation_advanced() {
    use paykit_sdk::UserConfirmed;

    let w = World::new();
    let (wallet, mut shop, mut second, _) = contemporaneous_publishers(&w).await;
    let payer = wallet.identity();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    assert_eq!(shown.version(), paykit_sdk::ListVersion::new(1, 3).unwrap());
    second.receive(None).await.unwrap();
    second
        .set_list_endpoints(offered("lno1third"))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let issue = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!(issue.counter, 5);
    let sent = published(&shop, &wallet).await;
    assert!(sent.last().unwrap().withdrawn);
    assert_eq!(sent.last().unwrap().version.counter(), 5);
}

#[tokio::test]
async fn test_publication_confirmed_withdrawal_after_two_moves_starts_the_current_generation() {
    use crate::common::BOB_ORIGIN;
    use paykit_sdk::UserConfirmed;
    use pubky_switch_core::records::Inbox;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    let mut service = w.bob.switch().clone();
    for generation in 2..=3 {
        let next = service.change_key_into([80 + generation as u8; 32]);
        w.router.add_switch(BOB_ORIGIN, &next);
        let moved = w
            .bob
            .sign_next(|body| {
                body.generation = generation;
                body.inbox = Inbox::new(BOB_ORIGIN, next.service_key());
            })
            .unwrap();
        next.deliver_identity(&moved).unwrap();
        shop.app_mut().refresh_identity().await.unwrap();
        shop.app_mut().follow_provider().await.unwrap();
        service = next;
    }
    shop.receive(None).await.unwrap();
    shop.send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap();
    let current = target(&shop, &payer, 3);
    let key = shop.app().record_key(&current).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let issue = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert_eq!((issue.generation, issue.counter), (3, 1));
    let sent = published(&shop, &buyer).await;
    assert!(sent.last().unwrap().withdrawn);
}

#[tokio::test]
async fn test_publication_stops_reporting_an_unknown_withdrawal_once_nothing_is_payable() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    // Past the earlier list's expiry nothing withdrawn can be paid any more.
    w.clock
        .advance_ms(paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS + crate::common::HOUR_MS);
    shop.receive(None).await.unwrap();
    shop.withdraw_list_from(payer).await.unwrap();
}

#[tokio::test]
async fn test_publication_takes_only_a_proof_naming_its_own_unknown_list_as_its_arrival() {
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    // The shop reads and edits its lists, so it can block the payer.
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "inbox".into(),
    });
    shop.publish_list_to(payer).await.unwrap();
    let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let list = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &list.msg_id)
        .await
        .unwrap());
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    let shop_app = shop.app_id();
    let proof = |app: [u8; 16], version: Option<paykit_sdk::ListVersion>| {
        let attempt = paykit_lib::AttemptId::generate().unwrap();
        let body = paykit_lib::bodies::PaymentProof {
            attempt_id: attempt,
            billing_period: None,
            payment_endpoint_identifier: crate::common::endpoint("btc-lightning-bolt12"),
            payment_app_id: app,
            list_version: version,
            amount: Some(btc("0.001")),
            allowance_id: None,
            label: None,
            payment_reference: None,
            // Each proof its own settlement, so none is placed on another's record.
            proof: serde_json::json!({ "rail": "fake", "tx": attempt.to_text() })
                .as_object()
                .cloned()
                .unwrap(),
        }
        .to_body(0)
        .unwrap();
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_app],
            "paykit.payment_proof",
            Vec::new(),
            body,
        );
    };
    // A proof naming no version, another version, or that version of another app's list
    // shows nothing of this list.
    for (app, version) in [
        (shop_app, None),
        (
            shop.app_id(),
            Some(paykit_sdk::ListVersion::new(1, 2).unwrap()),
        ),
        ([9; 16], Some(shown.version())),
    ] {
        proof(app, version);
        w.clock.advance_ms(1);
        shop.receive(None).await.unwrap();
        assert_eq!(
            unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
            shown,
            "{app:?} {version:?}"
        );
    }
    // Nor does one from a sender whose message is dropped as never processed: blocked,
    // its proof isn't taken, so it shows nothing either.
    shop.block(&payer).await.unwrap();
    proof(shop_app, Some(shown.version()));
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    assert_eq!(list_notices(&shop).await.len(), 1);
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(list_notices(&shop).await.len(), 1);
    // One naming this app's list at that version does.
    w.clock.advance_ms(1);
    proof(shop_app, Some(shown.version()));
    shop.receive(None).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    drop(buyer);
}

#[tokio::test]
async fn test_publication_takes_a_proof_paid_from_the_unknown_list_as_its_arrival() {
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    // The inbox takes the list but its answer is lost: the payer holds it, and the shop
    // can't tell.
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "inbox".into(),
    });
    shop.publish_list_to(payer).await.unwrap();
    let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let list = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &list.msg_id)
        .await
        .unwrap());
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    assert!(!shown.withdrawal());
    // The buyer pays from that list; its proof names the version it paid from.
    buyer.receive(None).await.unwrap();
    assert!(pays(&w, &mut buyer).await);
    shop.receive(None).await.unwrap();
    // It arrived: nothing is left of unknown outcome, nothing is sent in its place, and its
    // notice is gone.
    shop.publish_list_to(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert!(list_notices(&shop).await.is_empty());
}

#[tokio::test]
async fn test_publication_takes_a_list_request_naming_the_unknown_withdrawal_as_its_arrival() {
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    // The inbox takes the withdrawal but its answer is lost: the payer holds it.
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "inbox".into(),
    });
    shop.withdraw_list_from(payer).await.unwrap();
    let key = shop.app().record_key(&target(&shop, &payer, 1)).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let empty = ListPublicationRecord::read(held.content.value().unwrap()).unwrap();
    assert!(shop
        .app_mut()
        .withdraw_message("paykit", &empty.msg_id)
        .await
        .unwrap());
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    buyer.receive(None).await.unwrap();
    assert!(!pays(&w, &mut buyer).await);
    // A request naming that version but no app may name another app's counter: no evidence.
    let untargeted = paykit_lib::bodies::PrivatePaymentListRequest {
        held_version: Some(shown.version()),
        app_id: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        paykit_lib::Kind::PrivatePaymentListRequest.as_str(),
        Vec::new(),
        untargeted,
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err()),
        shown
    );
    // Its list request names the withdrawal's version: it arrived.
    let body = paykit_lib::bodies::PrivatePaymentListRequest {
        held_version: Some(shown.version()),
        app_id: Some(shop.app_id()),
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        paykit_lib::Kind::PrivatePaymentListRequest.as_str(),
        Vec::new(),
        body,
    );
    shop.receive(None).await.unwrap();
    shop.withdraw_list_from(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
}

/// The shop `shop`, holding `paykit_request` and `paykit_contacts` on `store`: it reads the
/// lists, so it can block the payer.
async fn contacts_shop(
    w: &World,
    store: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) -> Paykit {
    w.join_with(
        &w.bob,
        "shop",
        paykit_sdk::AppScopes::new()
            .request()
            .manage_contacts()
            .build()
            .unwrap(),
        paykit_sdk::PaykitConfig::default(),
        store,
    )
    .await
}

/// Alice's wallet's proof of a payment without a request to the shop app `shop`, of the fake
/// rail's settlement `tx`, naming `version` of that app's list. Returns its `msg_id`.
fn proof_naming(
    w: &World,
    shop: [u8; 16],
    version: Option<paykit_sdk::ListVersion>,
    tx: &str,
) -> String {
    let body = paykit_lib::bodies::PaymentProof {
        attempt_id: paykit_lib::AttemptId::generate().unwrap(),
        billing_period: None,
        payment_endpoint_identifier: crate::common::endpoint("btc-lightning-bolt12"),
        payment_app_id: shop,
        list_version: version,
        amount: Some(btc("0.001")),
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: serde_json::json!({ "rail": "fake", "tx": tx })
            .as_object()
            .cloned()
            .unwrap(),
    }
    .to_body(0)
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop],
        "paykit.payment_proof",
        Vec::new(),
        body,
    )
}

/// The shop's payee record of the payment without a request settled by the fake rail's `tx`.
fn settlement_record(tx: &str) -> RecordTarget {
    let collection = paykit_lib::collections::Collection::PayeeActivity;
    RecordTarget::new(
        Collection::new(collection.name(), collection.policy(None)),
        &natural_key::payee_payment_record(
            &crate::common::endpoint("btc-lightning-bolt12"),
            format!("fake:{tx}").as_bytes(),
        ),
    )
}

/// Whether the shop's record `target` holds the message `msg_id`.
async fn record_holds(shop: &Paykit, target: &RecordTarget, msg_id: &str) -> bool {
    let key = shop.app().record_key(target).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    String::from_utf8_lossy(held.content.value().unwrap()).contains(msg_id)
}

#[tokio::test]
async fn test_publication_proof_naming_an_unknown_withdrawal_leaves_it_unknown() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let mut shop = contacts_shop(&w, crate::common::store()).await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    shop.publish_list_to(payer).await.unwrap();
    let (_, key) = unknown_list(&w, &mut shop, payer, async |shop| {
        shop.withdraw_list_from(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err());
    assert!(shown.withdrawal());
    // Nothing is paid from an empty list, and a payer holding the list before it can name
    // the next counter without having it: the proof shows nothing (Paykit spec 9).
    w.clock.advance_ms(1);
    proof_naming(&w, shop.app_id(), Some(shown.version()), "t1");
    shop.receive(None).await.unwrap();
    assert_eq!(
        unknown_shown(shop.withdraw_list_from(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(list_notices(&shop).await.len(), 1);
    assert_eq!(w.record_writes(&w.bob, &key), 2);
}

/// Bob's shop, on a store showing no seen marks, holds an unknown list to alice; another of
/// bob's apps then writes the shop's record of alice's settlement `t1` with a value that
/// doesn't read. Returns the shop, the list shown and its record key.
async fn unknown_list_over_an_unread_settlement(
    w: &World,
) -> (Paykit, paykit_sdk::UnknownListDelivery, [u8; 32]) {
    use pubky_switch_client::app::OpBatch;

    let seen = std::sync::Arc::new(crate::outcome_record::NoSeenMarks::default());
    let base =
        std::sync::Arc::clone(&seen) as std::sync::Arc<dyn pubky_switch_client::store::StateStore>;
    let payer = w.alice_id();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            paykit_sdk::PaykitConfig::default(),
            base,
        )
        .await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (_, key) = unknown_list(w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    proof_naming(w, shop.app_id(), None, "t1");
    shop.receive(None).await.unwrap();
    seen.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    seller_wallet.receive(None).await.unwrap();
    let op = seller_wallet
        .app_mut()
        .record_op(
            &settlement_record("t1"),
            Some(crate::outcome_record::INVALID),
            Vec::new(),
        )
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.app().record_key(&settlement_record("t1")).unwrap();
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(crate::outcome_record::INVALID));
    (shop, shown, key)
}

/// The replies `shop` keeps before their record (`pr/`).
async fn kept_replies(shop: &Paykit) -> usize {
    shop.app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn test_publication_proof_kept_over_an_unread_settlement_record_settles_the_unknown_list() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, shown, key) = unknown_list_over_an_unread_settlement(&w).await;
    // The record can't show its counterparty: the proof is kept as a reply before its
    // record, and kept, it shows the list version it was paid from (Paykit spec 8.1, 9).
    w.clock.advance_ms(1);
    proof_naming(&w, shop.app_id(), Some(shown.version()), "t1");
    shop.receive(None).await.unwrap();
    assert_eq!(kept_replies(&shop).await, 1);
    shop.publish_list_to(payer).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert!(list_notices(&shop).await.is_empty());
}

#[tokio::test]
async fn test_publication_proof_dropped_to_make_room_leaves_the_list_unknown() {
    use pubky_switch_client::clock::Clock;

    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, shown, key) = unknown_list_over_an_unread_settlement(&w).await;
    // Alice's kept replies reach their bound, all accepted after her proof: the proof,
    // accepted earliest, is the one dropped, as never processed (Paykit spec 8.1).
    let at = w.clock.now_ms() + 1;
    w.clock.set_ms(at + 10);
    for i in 0..paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_request_acceptance",
            vec![[u8::try_from(i).unwrap(); 32]],
            serde_json::Map::new(),
        );
    }
    w.clock.set_ms(at);
    let dropped = proof_naming(&w, shop.app_id(), Some(shown.version()), "t1");
    w.clock.set_ms(at + 20);
    shop.receive(None).await.unwrap();
    assert_eq!(
        kept_replies(&shop).await,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER
    );
    let kept = shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .into_iter()
        .any(|(at, _)| String::from_utf8_lossy(&at).contains(&dropped));
    assert!(!kept);
    // Dropped, it shows nothing of the list.
    assert_eq!(
        unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(list_notices(&shop).await.len(), 1);
    assert_eq!(w.record_writes(&w.bob, &key), 1);
}

/// The shop's list to alice ends unknown, and her proof of settlement `t1`, naming no list
/// version, opens the shop's record of it. Returns the shop, the list shown and its key.
async fn unknown_list_after_a_settlement(
    w: &World,
    payer: [u8; 32],
) -> (Paykit, paykit_sdk::UnknownListDelivery, [u8; 32]) {
    let mut shop = contacts_shop(w, crate::common::store()).await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (_, key) = unknown_list(w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    shop.receive(None).await.unwrap();
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    proof_naming(w, shop.app_id(), None, "t1");
    shop.receive(None).await.unwrap();
    (shop, shown, key)
}

#[tokio::test]
async fn test_publication_blocked_proof_of_a_held_settlement_leaves_the_list_unknown() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, shown, key) = unknown_list_after_a_settlement(&w, payer).await;
    // A blocked payer's proof of a settlement the shop holds updates that record, but its
    // sender isn't admitted: it shows nothing of the list (Paykit spec 8.1, 9).
    shop.block(&payer).await.unwrap();
    w.clock.advance_ms(1);
    let again = proof_naming(&w, shop.app_id(), Some(shown.version()), "t1");
    shop.receive(None).await.unwrap();
    assert!(record_holds(&shop, &settlement_record("t1"), &again).await);
    assert_eq!(list_notices(&shop).await.len(), 1);
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(w.record_writes(&w.bob, &key), 1);
}

#[tokio::test]
async fn test_publication_proof_of_a_held_settlement_while_the_lists_are_unknown_leaves_the_list_unknown(
) {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, shown, key) = unknown_list_after_a_settlement(&w, payer).await;
    // The due read of the lists fails: whether the payer is blocked isn't known, so its
    // proof updating the held record shows nothing of the list (Paykit spec 8.1, 9).
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/allow".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let again = proof_naming(&w, shop.app_id(), Some(shown.version()), "t1");
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_none());
    assert!(record_holds(&shop, &settlement_record("t1"), &again).await);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        unknown_shown(shop.publish_list_to(payer).await.unwrap_err()),
        shown
    );
    assert_eq!(list_notices(&shop).await.len(), 1);
    assert_eq!(w.record_writes(&w.bob, &key), 1);
}

/// The shop's list to the payer ends unknown, the user confirms sending it again while
/// sends to the payer are held, and the confirmation waits. Returns the shop, the list
/// shown and its record key.
async fn confirmed_while_blocked(
    w: &World,
    payer: [u8; 32],
) -> (Paykit, paykit_sdk::UnknownListDelivery, [u8; 32]) {
    use paykit_sdk::{Refusal, UserConfirmed};

    let mut shop = contacts_shop(w, crate::common::store()).await;
    shop.app_mut().invite(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_list_endpoints(offered("lno1first")).await.unwrap();
    let (_, key) = unknown_list(w, &mut shop, payer, async |shop| {
        shop.publish_list_to(payer).await.unwrap();
    })
    .await;
    let shown = unknown_shown(shop.publish_list_to(payer).await.unwrap_err());
    shop.block(&payer).await.unwrap();
    let refused = shop
        .send_list_again(&shown, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    assert_eq!(shop.lists_waiting_to_send_again().await.unwrap(), [payer]);
    (shop, shown, key)
}

#[tokio::test]
async fn test_publication_waiting_confirmation_leaves_the_list_once_the_earlier_list_arrived() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, shown, _) = confirmed_while_blocked(&w, payer).await;
    shop.unblock(&payer).await.unwrap();
    // The payer's list request names the earlier version: it arrived, so nothing waits to
    // be sent again, before any publication runs (Paykit spec 9).
    w.clock.advance_ms(1);
    ask_shop(&w, &shop, Some(shown.version()));
    shop.app_mut().receive(None).await.unwrap();
    assert!(shop.lists_waiting_to_send_again().await.unwrap().is_empty());
    shop.receive(None).await.unwrap();
    assert!(shop.lists_waiting_to_send_again().await.unwrap().is_empty());
    shop.publish_list_to(payer).await.unwrap();
}

#[tokio::test]
async fn test_publication_waiting_confirmation_stays_after_the_earlier_list_expired() {
    let w = World::new();
    let buyer = w.wallet("wallet").await;
    let payer = buyer.identity();
    let (mut shop, _, key) = confirmed_while_blocked(&w, payer).await;
    // Nothing can be paid from the earlier list any more, but the user's confirmation is
    // still kept, and still sent once it can be.
    w.clock
        .advance_ms(paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS + crate::common::HOUR_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.lists_waiting_to_send_again().await.unwrap(), [payer]);
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 2);
    assert!(shop.lists_waiting_to_send_again().await.unwrap().is_empty());
}
