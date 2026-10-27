use super::*;

const REFUSAL: &str = "cannot restore a backup over existing SDK-managed state";

fn capabilities() -> paykit_lib::PaykitAppCapabilities {
    paykit_lib::PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    }
}

fn public_endpoint_record() -> PublicEndpointRecord {
    PublicEndpointRecord {
        app_id: app_id(),
        identifier: "btc-lightning-bolt11".into(),
        payload: Some("ln-public-payload".into()),
        status: crate::PublicationStatus::Published,
        updated_at: timestamp(),
        last_error: None,
    }
}

/// Storage holding the records `publish_paykit_app` writes after a successful
/// registry publication. Publishing itself needs a live homeserver, so these
/// tests seed the same storage writes directly.
async fn published_app_storage(local: &PubkyPublicKey) -> InMemoryStorage {
    let storage = InMemoryStorage::new();
    storage
        .transaction({
            let local = local.clone();
            move |tx| {
                tx.save_identity_state(identity(local));
                tx.save_paykit_app_capabilities(&app_id(), capabilities());
                tx.activate_paykit_app(&app_id());
                Ok(())
            }
        })
        .await
        .unwrap();
    storage
}

async fn refusal_context(storage: &InMemoryStorage, local: &PubkyPublicKey) -> String {
    let before = storage.snapshot().unwrap();
    let error = restore_backup_state(storage, empty_backup(identity(local.clone())))
        .await
        .unwrap_err();
    assert_eq!(storage.snapshot().unwrap(), before);
    match error {
        PaykitSdkError::Policy {
            context,
            source: None,
        } => context,
        other => panic!("unexpected restore error: {other}"),
    }
}

fn assert_no_identifiers(context: &str, local: &PubkyPublicKey) {
    for value in [
        local.to_string(),
        app_id().as_str().to_owned(),
        "btc-lightning-bolt11".into(),
        "ln-public-payload".into(),
    ] {
        assert!(!context.contains(&value), "{context} leaks {value}");
    }
    assert!(!context.chars().any(|c| c.is_ascii_digit()), "{context}");
}

#[tokio::test]
async fn test_restore_blockers_allow_identity_authorization_cache_and_leases() {
    let local = public_key();
    let storage = InMemoryStorage::new();
    storage
        .transaction({
            let local = local.clone();
            move |tx| {
                tx.save_identity_state(identity(local.clone()));
                tx.save_authorized_paykit_apps(local, HashMap::from([(app_id(), capabilities())]));
                let expires_at = timestamp() + chrono::Duration::seconds(60);
                tx.claim_peer_link_operation(&public_key(), timestamp(), expires_at)?;
                tx.claim_paykit_app_operation(&app_id(), timestamp(), expires_at)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let mut state = storage.snapshot().unwrap();
    state.paykit_noise_public_key = Some(public_key());
    assert_eq!(state.next_peer_link_operation_lease_id, 1);
    assert_eq!(state.next_paykit_app_operation_lease_id, 1);

    assert!(restore_blockers(&StorageState::default()).is_empty());
    assert!(restore_blockers(&state).is_empty());
}

#[test]
fn test_restore_blockers_name_app_setup_records() {
    let mut state = StorageState::default();
    state
        .registered_paykit_app_capabilities
        .insert(app_id(), capabilities());
    assert_eq!(restore_blockers(&state), ["registered Paykit Apps"]);

    state.registered_paykit_apps.insert(app_id());
    assert_eq!(restore_blockers(&state), ["registered Paykit Apps"]);

    state.public_endpoint_records.insert(
        (app_id(), "btc-lightning-bolt11".into()),
        public_endpoint_record(),
    );
    assert_eq!(
        restore_blockers(&state),
        ["registered Paykit Apps", "public Payment Endpoint records"]
    );
}

#[test]
fn test_restore_blockers_treat_advanced_counters_as_history() {
    type Mutation = fn(&mut StorageState);
    let cases: [(Mutation, &str); 5] = [
        (
            |state| state.next_outbound_private_message_id = 1,
            "outbound Private Application Messages",
        ),
        (
            |state| state.next_receive_batch_id = 1,
            "received Private Application Messages",
        ),
        (
            |state| state.next_private_stream_item_id = 1,
            "received Private Application Messages",
        ),
        (
            |state| {
                state.retired_paykit_apps.insert(app_id());
            },
            "retired Paykit Apps",
        ),
        (
            |state| {
                let contact = contact_record(public_key());
                state
                    .contact_records
                    .insert(contact.public_key.clone(), contact);
            },
            "Contact Records",
        ),
    ];
    for (mutate, expected) in cases {
        let mut state = StorageState::default();
        mutate(&mut state);
        assert_eq!(restore_blockers(&state), [expected]);
    }
}

#[tokio::test]
async fn test_restore_backup_state_refusal_names_registered_paykit_apps() {
    let local = public_key();
    let storage = published_app_storage(&local).await;

    let context = refusal_context(&storage, &local).await;

    assert_eq!(context, format!("{REFUSAL}: registered Paykit Apps"));
    assert_no_identifiers(&context, &local);
}

#[tokio::test]
async fn test_restore_backup_state_refusal_names_public_endpoint_records() {
    let local = public_key();
    let storage = published_app_storage(&local).await;
    storage
        .transaction(|tx| {
            tx.save_public_endpoint_record(public_endpoint_record());
            Ok(())
        })
        .await
        .unwrap();

    let context = refusal_context(&storage, &local).await;

    assert_eq!(
        context,
        format!("{REFUSAL}: registered Paykit Apps, public Payment Endpoint records")
    );
    assert_no_identifiers(&context, &local);
}

#[tokio::test]
async fn test_restore_backup_state_accepts_fresh_backing_before_publication() {
    let local = public_key();
    let storage = InMemoryStorage::new();
    storage
        .transaction({
            let local = local.clone();
            move |tx| {
                tx.save_identity_state(identity(local));
                Ok(())
            }
        })
        .await
        .unwrap();
    let mut backup = empty_backup(identity(local));
    backup
        .public_endpoint_records
        .push(public_endpoint_record());

    let report = restore_backup_state(&storage, backup).await.unwrap();

    assert!(report.restored_identity);
    assert_eq!(storage.snapshot().unwrap().public_endpoint_records.len(), 1);
}
