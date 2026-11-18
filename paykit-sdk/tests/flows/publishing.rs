//! A merchant updates the reusable endpoint it shares with a chosen contact.

use crate::common::{btc, endpoint, World};
use paykit_lib::derive::natural_key;
use paykit_lib::records::ListPublicationRecord;
use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{PayOutcome, Paykit, UnrequestedPayment};
use pubky_switch_client::app::{Collection, KeptReason, RecordRead, RecordTarget};
use pubky_switch_core::ident::Policy;

fn target(shop: &Paykit, recipient: &[u8; 32], generation: u64) -> RecordTarget {
    let policy = Policy::new("paykit_request", &["paykit_request"]).permanent(Some(generation));
    RecordTarget::new(
        Collection::new("private_list_publications", policy),
        &natural_key::list_publication(&shop.app_id(), recipient, generation),
    )
}

fn offered(
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
