use super::*;

fn empty_backup_state() -> SdkBackupState {
    SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
        version: crate::SDK_BACKUP_VERSION,
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    }
}

#[tokio::test]
async fn test_restore_backup_state_requires_active_identity() {
    let storage = InMemoryStorage::new();
    let existing_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(existing_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let backup_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
        version: crate::SDK_BACKUP_VERSION,
        identity_state: Some(IdentityState {
            public_key: Some(backup_public_key),
            initialized_at: FixedClock.now(),
        }),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .restore_backup_state(backup, RestoredLinkPolicy::Resume)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage.snapshot().unwrap().identity_state.is_some());
}

#[tokio::test]
async fn test_restore_backup_state_rejects_concurrent_identity_operation() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk
        .restore_backup_state(empty_backup_state(), RestoredLinkPolicy::Resume)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_recover_shared_state_requires_identity_and_exclusive_operation() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let replacement = crate::PaykitIdentitySecretKey::new([8; 32], 2).unwrap();
    assert!(matches!(
        sdk.recover_shared_state_from_backup(empty_backup_state(), replacement.clone())
            .await,
        Err(PaykitSdkError::Identity { .. })
    ));
    let _guard = sdk.claim_identity_operation("test operation").unwrap();
    assert!(matches!(
        sdk.recover_shared_state_from_backup(empty_backup_state(), replacement)
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));
}

/// Linked identity whose Encrypted Link checkpoint travels in a backup.
struct RestoredLinkFixture {
    local: PubkyPublicKey,
    local_noise: PubkyPublicKey,
    counterparty: PubkyPublicKey,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
}

impl RestoredLinkFixture {
    fn new() -> Self {
        let signer = pubky::Keypair::random();
        Self {
            local: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            local_noise: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            counterparty: PubkyPublicKey::from_public_key(&signer.public_key()),
            authorization: paykit_lib::PaykitNoiseKeyAuthorization::sign(&signer, &[7; 32], 1)
                .unwrap(),
        }
    }

    fn identity(&self) -> IdentityState {
        IdentityState {
            public_key: Some(self.local.clone()),
            initialized_at: FixedClock.now(),
        }
    }

    /// Established link checkpoint with a valid snapshot and a matching pin.
    fn link_checkpoint(&self, generation: u64) -> EncryptedLinkStateRecord {
        use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

        let state = pubky_noise::serializer::PubkyNoiseSessionState {
            version: pubky_noise::serializer::SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternXX,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: Some([2; 32]),
            counter: 3,
            noise_step: NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([3; 32]),
            link_id: Some([4; 32]),
            sending_nonce: 0,
            receiving_nonce: 0,
            write_counter: 3,
            read_counter: 3,
            endpoint_pubkey: self.counterparty.to_public_key().unwrap().to_bytes(),
            handshake_messages: vec![vec![5; 96]],
        };
        let mut bytes = state.serialize();
        bytes.extend_from_slice(&self.authorization.noise_public_key().to_bytes());
        // The Paykit snapshot appends the routing key and empty recovery context.
        bytes.extend_from_slice(&[0; 72]);
        EncryptedLinkStateRecord {
            counterparty: self.counterparty.clone(),
            link_snapshot: Some(bytes),
            handshake_snapshot: None,
            handshake_role: None,
            generation,
            checkpointed_at: FixedClock.now(),
        }
    }

    async fn seed(&self, storage: &InMemoryStorage, link: EncryptedLinkStateRecord) {
        let identity = self.identity();
        let local_noise = self.local_noise.clone();
        let peer = LinkedPeerRecord {
            counterparty: self.counterparty.clone(),
            state: LinkedPeerState::Linked,
            last_sync_at: Some(FixedClock.now()),
            last_private_receive_at: None,
            failure_count: 0,
            local_recovery_attempt_id: None,
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: None,
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
            noise_key_authorization: Some(self.authorization.clone()),
        };
        storage
            .transaction(move |tx| {
                tx.save_identity_state(identity);
                tx.save_paykit_noise_public_key(local_noise);
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(link);
                Ok(())
            })
            .await
            .unwrap();
    }

    /// The restore call the runtime makes after validating the live session.
    async fn restore(
        &self,
        storage: &InMemoryStorage,
        backup: SdkBackupState,
        link_policy: RestoredLinkPolicy,
    ) -> Result<RestoreReport> {
        restore_sdk_backup_state(
            storage,
            backup,
            Some(self.identity()),
            Some(self.local_noise.clone()),
            FixedClock.now(),
            link_policy,
        )
        .await
    }
}

#[tokio::test]
async fn test_restore_backup_state_require_recovery_blocks_private_sends() {
    let fixture = RestoredLinkFixture::new();
    let exported_from = InMemoryStorage::new();
    let checkpoint = fixture.link_checkpoint(2);
    fixture.seed(&exported_from, checkpoint.clone()).await;
    let backup = export_sdk_backup_state(&exported_from).await.unwrap();
    // Private sends made after the export are unknown to the backup, so the
    // caller of an old or uncertain backup asks for link recovery.
    let storage = InMemoryStorage::new();

    let report = fixture
        .restore(&storage, backup, RestoredLinkPolicy::RequireRecovery)
        .await
        .unwrap();

    let restored = storage.snapshot().unwrap();
    let link = &restored.encrypted_link_states[&fixture.counterparty];
    assert_eq!(
        restored.linked_peers[&fixture.counterparty].state,
        LinkedPeerState::RecoveryRequired
    );
    assert!(link.link_snapshot.is_none());
    assert_eq!(link.generation, checkpoint.generation + 1);
    assert_eq!(
        report.recovery_required_peers,
        vec![fixture.counterparty.clone()]
    );

    // New private work for the restored peer is neither selected nor sent.
    let counterparty = fixture.counterparty.clone();
    storage
        .transaction(move |tx| {
            // Restore clears app registration; apps republish before new work.
            tx.activate_paykit_app(&app_id());
            tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                app_id(),
                "paykit.private_payment_list".into(),
                private_list_json(),
                FixedClock.now(),
            ))?;
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        sdk.process_outbound_private_messages(fixture.counterparty.clone())
            .await,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_restore_over_existing_link_state() {
    let fixture = RestoredLinkFixture::new();
    let exported_from = InMemoryStorage::new();
    fixture
        .seed(&exported_from, fixture.link_checkpoint(2))
        .await;
    let backup = export_sdk_backup_state(&exported_from).await.unwrap();
    // The live link has checkpointed since the backup was exported.
    let storage = InMemoryStorage::new();
    fixture.seed(&storage, fixture.link_checkpoint(4)).await;
    let before = storage.snapshot().unwrap();

    for link_policy in [
        RestoredLinkPolicy::Resume,
        RestoredLinkPolicy::RequireRecovery,
    ] {
        let result = fixture.restore(&storage, backup.clone(), link_policy).await;

        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(storage.snapshot().unwrap(), before);
    }
}
