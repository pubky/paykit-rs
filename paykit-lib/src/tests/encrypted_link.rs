use super::*;

#[tokio::test]
async fn test_handshake_snapshot_serialize_roundtrip() {
    let InProgressHandshakeSetup {
        _testnet,
        initiator_session,
        responder_session,
        initiator_handshake,
        responder_handshake: _responder_handshake,
    } = InProgressHandshakeSetup::new().await;

    let snapshot = initiator_handshake.snapshot().unwrap();
    let bytes = snapshot.serialize();
    assert_eq!(bytes.len(), 229, "snapshot should be 229 bytes");

    let restored_snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&bytes).unwrap();
    assert_eq!(
        restored_snapshot.recipient(),
        snapshot.recipient(),
        "recipient public key should survive serialize/deserialize"
    );
    assert_eq!(
        restored_snapshot.remote_noise_public_key(),
        snapshot.remote_noise_public_key(),
        "remote Noise public key should survive serialize/deserialize"
    );

    let bytes2 = restored_snapshot.serialize();
    assert_eq!(
        bytes, bytes2,
        "double round-trip should produce identical bytes"
    );

    initiator_session.signout().await.unwrap();
    responder_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_handshake_restore_and_complete() {
    let InProgressHandshakeSetup {
        _testnet,
        initiator_session,
        responder_session,
        mut initiator_handshake,
        responder_handshake,
    } = InProgressHandshakeSetup::new().await;

    // Set a non-default value before snapshotting to verify restore resets
    // this knob back to the default.
    initiator_handshake.set_max_recovery_attempts(99);

    // Advance both sides once so snapshots capture an in-flight handshake.
    let initiator_handshake = match advance_handshake(initiator_handshake).await.unwrap() {
        HandshakeProgress::Pending(h) => h,
        HandshakeProgress::Complete(_) => {
            panic!("initiator handshake unexpectedly completed in one step")
        }
    };
    let responder_handshake = match advance_handshake(responder_handshake).await.unwrap() {
        HandshakeProgress::Pending(h) => h,
        HandshakeProgress::Complete(_) => {
            panic!("responder handshake unexpectedly completed in one step")
        }
    };

    let initiator_config = initiator_handshake.config().clone();
    let responder_config = responder_handshake.config().clone();

    let initiator_snapshot_bytes = initiator_handshake.serialize().unwrap();
    let responder_snapshot_bytes = responder_handshake.serialize().unwrap();
    let initiator_snapshot =
        EncryptedLinkHandshakeSnapshot::deserialize(&initiator_snapshot_bytes).unwrap();
    let responder_snapshot =
        EncryptedLinkHandshakeSnapshot::deserialize(&responder_snapshot_bytes).unwrap();

    let initiator_remote = initiator_snapshot.recipient().clone();
    let responder_remote = responder_snapshot.recipient().clone();

    let restored_initiator = restore_encrypted_link_handshake_from_config(
        initiator_config,
        &initiator_remote,
        initiator_snapshot,
    )
    .await
    .unwrap();
    let restored_responder = restore_encrypted_link_handshake_from_config(
        responder_config,
        &responder_remote,
        responder_snapshot,
    )
    .await
    .unwrap();

    assert_eq!(restored_initiator.recovery_attempts_for_test(), 0);
    assert_eq!(
        restored_initiator.max_recovery_attempts_for_test(),
        DEFAULT_MAX_RECOVERY_ATTEMPTS
    );

    let (mut initiator_link, mut responder_link) = tokio::join!(
        drive_handshake_to_completion(restored_initiator),
        drive_handshake_to_completion(restored_responder),
    );

    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("lnrestored..."),
    );
    set_private_payment_list(
        &mut initiator_link,
        &private_payment_list(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = receive_latest_private_payment_list_for_test(&mut responder_link)
        .await
        .unwrap();
    assert_eq!(received.payment_endpoints.len(), 1);
    assert_eq!(
        received
            .payment_endpoints
            .get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
        Some(&PaymentEndpointPayload::new("lnrestored..."))
    );

    close_encrypted_link(initiator_link).await.unwrap();
    close_encrypted_link(responder_link).await.unwrap();
    initiator_session.signout().await.unwrap();
    responder_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_handshake_restore_rejects_mismatched_remote_pubkey() {
    let InProgressHandshakeSetup {
        _testnet,
        initiator_session,
        responder_session,
        initiator_handshake,
        responder_handshake: _responder_handshake,
    } = InProgressHandshakeSetup::new().await;

    let snapshot = initiator_handshake.snapshot().unwrap();
    let config = initiator_handshake.config().clone();
    let wrong_remote = initiator_session.info().public_key().clone();

    let result =
        restore_encrypted_link_handshake_from_config(config, &wrong_remote, snapshot).await;
    let err = match result {
        Ok(_) => panic!("restore should reject mismatched remote pubkey"),
        Err(err) => err,
    };
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match snapshot recipient")),
        "expected Validation mismatch error, got: {err}"
    );

    initiator_session.signout().await.unwrap();
    responder_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_handshake_restore_rejects_transport_phase_snapshot() {
    let setup = PrivateTestSetup::new().await;

    // Build a handshake snapshot value from a transport-mode link snapshot.
    let transport_bytes = setup.sender_link.serialize().unwrap();
    let handshake_snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&transport_bytes).unwrap();
    let sender_config = setup.sender_link.config().clone();
    let remote = handshake_snapshot.recipient().clone();

    let result =
        restore_encrypted_link_handshake_from_config(sender_config, &remote, handshake_snapshot)
            .await;
    let err = match result {
        Ok(_) => panic!("handshake restore should reject transport-phase snapshot"),
        Err(err) => err,
    };
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("handshake-phase snapshot")),
        "expected handshake-phase validation error, got: {err}"
    );

    close_encrypted_link(setup.sender_link).await.unwrap();
    close_encrypted_link(setup.receiver_link).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_handshake_snapshot_deserialize_rejects_garbage() {
    let result = EncryptedLinkHandshakeSnapshot::deserialize(&[0u8; 10]);
    assert!(result.is_err(), "deserializing garbage should fail");
    let err = result.unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { .. }),
        "expected InvalidData error, got: {err}"
    );
}

#[tokio::test]
async fn test_handshake_snapshot_deserialize_rejects_wrong_length() {
    let result = EncryptedLinkHandshakeSnapshot::deserialize(&[0u8; 189]);
    assert!(
        matches!(result, Err(PaykitError::InvalidData { .. })),
        "snapshots with the wrong serialized length should fail"
    );
}

fn transport_snapshot_state_with_nonces(
    sending_nonce: u64,
    receiving_nonce: u64,
) -> pubky_noise::serializer::PubkyNoiseSessionState {
    pubky_noise::serializer::PubkyNoiseSessionState {
        version: pubky_noise::serializer::SESSION_STATE_VERSION,
        phase: pubky_noise::snow_crypto::NoisePhase::Transport,
        pattern: pubky_noise::snow_crypto::HandshakePattern::PatternXX,
        initiator: true,
        ephemeral_secret: [1; 32],
        static_secret: Some([2; 32]),
        counter: 2,
        noise_step: pubky_noise::snow_crypto::NoiseStep::Final,
        sub_step_index: 0,
        handshake_hash: Some([3; 32]),
        link_id: Some([4; 32]),
        sending_nonce,
        receiving_nonce,
        write_counter: 3,
        read_counter: 3,
        endpoint_pubkey: Keypair::random().public_key().as_inner().to_bytes(),
    }
}

#[tokio::test]
async fn test_encrypted_link_snapshot_serialize_roundtrip() {
    let mut setup = PrivateTestSetup::new().await;

    // Send a message to advance nonces beyond zero.
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("ln..."),
    );
    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints),
    )
    .await
    .unwrap();

    // Take a snapshot and serialize.
    let snapshot = setup.sender_link.snapshot().unwrap();
    let bytes = snapshot.serialize();
    assert_eq!(bytes.len(), 229, "snapshot should be 229 bytes");

    // Deserialize and verify the recipient is reconstructed correctly.
    let restored_snapshot = EncryptedLinkSnapshot::deserialize(&bytes).unwrap();
    assert_eq!(
        restored_snapshot.recipient(),
        snapshot.recipient(),
        "recipient public key should survive serialize/deserialize"
    );
    assert_eq!(
        restored_snapshot.remote_noise_public_key(),
        snapshot.remote_noise_public_key(),
        "remote Noise public key should survive serialize/deserialize"
    );

    // Re-serialize and verify byte-level equality.
    let bytes2 = restored_snapshot.serialize();
    assert_eq!(
        bytes, bytes2,
        "double round-trip should produce identical bytes"
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_encrypted_link_restore_and_continue() {
    let mut setup = PrivateTestSetup::new().await;

    // Send a message before snapshotting.
    let mut payment_endpoints_v1 = HashMap::new();
    payment_endpoints_v1.insert(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("lnv1..."),
    );
    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints_v1),
    )
    .await
    .unwrap();

    // Consume the message on the receiver side.
    let received_v1 = receive_latest_private_payment_list_for_test(&mut setup.receiver_link)
        .await
        .unwrap();
    assert_eq!(received_v1.payment_endpoints.len(), 1);

    // Snapshot both sides after the first exchange.
    let sender_snapshot = setup.sender_link.snapshot().unwrap();
    let receiver_snapshot = setup.receiver_link.snapshot().unwrap();

    // Serialize and deserialize (simulating persistence).
    let sender_bytes = sender_snapshot.serialize();
    let receiver_bytes = receiver_snapshot.serialize();
    let sender_state = EncryptedLinkSnapshot::deserialize(&sender_bytes).unwrap();
    let receiver_state = EncryptedLinkSnapshot::deserialize(&receiver_bytes).unwrap();

    // Restore both sides using the in-process config variant.
    let sender_config = setup.sender_link.config().clone();
    let receiver_config = setup.receiver_link.config().clone();
    let sender_recipient = sender_state.recipient().clone();
    let receiver_recipient = receiver_state.recipient().clone();

    let mut restored_sender =
        restore_encrypted_link_from_config(sender_config, &sender_recipient, sender_state)
            .await
            .unwrap();
    let mut restored_receiver =
        restore_encrypted_link_from_config(receiver_config, &receiver_recipient, receiver_state)
            .await
            .unwrap();

    // Send a new message from the restored sender.
    let mut payment_endpoints_v2 = HashMap::new();
    payment_endpoints_v2.insert(
        PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap(),
        PaymentEndpointPayload::new("bc1pv2..."),
    );
    set_private_payment_list(
        &mut restored_sender,
        &private_payment_list(&payment_endpoints_v2),
    )
    .await
    .unwrap();

    // Receive on the restored receiver.
    let received_v2 = receive_latest_private_payment_list_for_test(&mut restored_receiver)
        .await
        .unwrap();
    assert_eq!(received_v2.payment_endpoints.len(), 1);
    assert_eq!(
        received_v2
            .payment_endpoints
            .get(&PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap()),
        Some(&PaymentEndpointPayload::new("bc1pv2...")),
    );

    // Clean up.
    close_encrypted_link(restored_sender).await.unwrap();
    close_encrypted_link(restored_receiver).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_receive_private_application_messages_returns_full_stream() {
    let mut setup = PrivateTestSetup::new().await;

    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("ln..."),
    );
    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints),
    )
    .await
    .unwrap();

    let unknown_json =
        r#"{"version":1,"kind":"paykit.test_unknown","payload":{"note":"preserve me"}}"#;
    let receipt_access_json =
        r#"{"version":1,"kind":"paykit.receipt_access","payload":"raw-only"}"#;

    send_raw_private_application_message(&mut setup.sender_link, unknown_json).await;
    send_raw_private_application_message(&mut setup.sender_link, receipt_access_json).await;

    let messages = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages
            .iter()
            .map(|message| message.kind.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("paykit.private_payment_list"),
            Some("paykit.test_unknown"),
            Some("paykit.receipt_access")
        ]
    );
    assert_eq!(messages[1].raw_json, unknown_json);
    assert_eq!(messages[2].raw_json, receipt_access_json);

    let empty = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert!(empty.is_empty());

    close_encrypted_link(setup.receiver_link).await.unwrap();
    close_encrypted_link(setup.sender_link).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_encrypted_link_restore_rejects_mismatched_remote_pubkey() {
    let setup = PrivateTestSetup::new().await;

    let snapshot = setup.sender_link.snapshot().unwrap();
    let sender_config = setup.sender_link.config().clone();
    let wrong_remote = setup.sender_session.info().public_key().clone();

    let result = restore_encrypted_link_from_config(sender_config, &wrong_remote, snapshot).await;
    let err = match result {
        Ok(_) => panic!("restore should reject mismatched remote pubkey"),
        Err(err) => err,
    };
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match snapshot recipient")),
        "expected Validation mismatch error, got: {err}"
    );

    close_encrypted_link(setup.sender_link).await.unwrap();
    close_encrypted_link(setup.receiver_link).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_encrypted_link_serialize_convenience() {
    let setup = PrivateTestSetup::new().await;

    // The convenience method should produce the same bytes as snapshot().serialize().
    let via_snapshot = setup.sender_link.snapshot().unwrap().serialize();
    let via_convenience = setup.sender_link.serialize().unwrap();
    assert_eq!(
        via_snapshot, via_convenience,
        "serialize() should equal snapshot().serialize()"
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_prepared_private_message_advances_only_after_persistence_acknowledgement() {
    let mut setup = PrivateTestSetup::new().await;
    let raw_json = r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#;

    let sender_before = setup.sender_link.serialize().unwrap();
    let prepared_send = setup
        .sender_link
        .prepare_private_application_message_json(raw_json)
        .unwrap();
    let sender_after = prepared_send.resulting_snapshot().serialize();
    let destination_path = prepared_send.destination_path().to_owned();
    let ciphertext = prepared_send.ciphertext().to_vec();
    let prepared_debug = format!("{prepared_send:?}");

    assert!(!prepared_debug.contains(&destination_path));
    assert!(setup.sender_link.serialize().is_err());
    assert_ne!(sender_after, sender_before);
    setup
        .sender_link
        .acknowledge_persisted_private_send(prepared_send)
        .unwrap();
    assert_eq!(setup.sender_link.serialize().unwrap(), sender_after);
    setup
        .sender_link
        .publish_prepared_private_application_message(&destination_path, &ciphertext)
        .await
        .unwrap();

    let receiver_before = setup.receiver_link.serialize().unwrap();
    let prepared_receive = setup
        .receiver_link
        .prepare_next_private_application_message()
        .await
        .unwrap()
        .expect("prepared message should be available");
    let receiver_after = prepared_receive.resulting_snapshot().serialize();

    assert_eq!(prepared_receive.message().raw_json, raw_json);
    assert!(setup.receiver_link.serialize().is_err());
    assert_ne!(receiver_after, receiver_before);
    setup
        .receiver_link
        .acknowledge_persisted_private_receive(prepared_receive)
        .unwrap();
    assert_eq!(setup.receiver_link.serialize().unwrap(), receiver_after);

    close_encrypted_link(setup.sender_link).await.unwrap();
    close_encrypted_link(setup.receiver_link).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn test_encrypted_link_snapshot_deserialize_rejects_garbage() {
    let result = EncryptedLinkSnapshot::deserialize(&[0u8; 10]);
    assert!(result.is_err(), "deserializing garbage should fail");
    let err = result.unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { .. }),
        "expected InvalidData error, got: {err}"
    );
}

#[tokio::test]
async fn test_encrypted_link_snapshot_deserialize_rejects_wrong_length() {
    let result = EncryptedLinkSnapshot::deserialize(&[0u8; 189]);
    assert!(
        matches!(result, Err(PaykitError::InvalidData { .. })),
        "snapshots with the wrong serialized length should fail"
    );
}

#[test]
fn test_encrypted_link_snapshot_deserialize_accepts_max_usable_noise_nonce() {
    let state = transport_snapshot_state_with_nonces(u64::MAX - 1, u64::MAX - 1);
    let mut bytes = state.serialize();
    bytes.extend_from_slice(&Keypair::random().public_key().as_inner().to_bytes());

    let snapshot = EncryptedLinkSnapshot::deserialize(&bytes).unwrap();

    assert_eq!(snapshot.serialize(), bytes);
}

#[test]
fn test_encrypted_link_snapshot_deserialize_rejects_reserved_noise_nonce() {
    for (sending_nonce, receiving_nonce) in [(u64::MAX, 0), (0, u64::MAX)] {
        let bytes =
            transport_snapshot_state_with_nonces(sending_nonce, receiving_nonce).serialize();
        let mut bytes = bytes;
        bytes.extend_from_slice(&Keypair::random().public_key().as_inner().to_bytes());

        assert!(
            matches!(
                EncryptedLinkSnapshot::deserialize(&bytes),
                Err(PaykitError::InvalidData { .. })
            ),
            "reserved Noise nonce should be rejected"
        );
    }
}
