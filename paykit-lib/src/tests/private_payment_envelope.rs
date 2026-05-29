use super::*;

const TEST_RECEIPT_ACCESS_JSON: &str = r#"{"version":1,"kind":"paykit.receipt_access","reference":"550e8400-e29b-41d4-a716-446655440000"}"#;

#[tokio::test]
async fn private_payment_envelope_empty_returns_empty() {
    let mut setup = PrivateTestSetup::new().await;

    let result = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "fresh link with no messages should return no payload"
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_envelope_round_trip() {
    let mut setup = PrivateTestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());

    let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    set_private_payment_envelope(
        &mut setup.sender_link,
        &PrivatePaymentEnvelope::new(reference.clone(), payment_endpoints),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.reference, reference);
    assert_eq!(received.payment_endpoints.len(), 1);
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_envelope_multiple_methods() {
    let mut setup = PrivateTestSetup::new().await;

    let lightning = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let onchain = PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap();
    let cashu = PaymentEndpointIdentifier::new("cashu-mint_id").unwrap();

    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(lightning.clone(), PaymentEndpointPayload::new("ln..."));
    payment_endpoints.insert(onchain.clone(), PaymentEndpointPayload::new("bc1p..."));
    payment_endpoints.insert(
        cashu.clone(),
        PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"),
    );

    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.payment_endpoints.len(), 3);
    assert_eq!(
        received.payment_endpoints.get(&lightning),
        Some(&PaymentEndpointPayload::new("ln..."))
    );
    assert_eq!(
        received.payment_endpoints.get(&onchain),
        Some(&PaymentEndpointPayload::new("bc1p..."))
    );
    assert_eq!(
        received.payment_endpoints.get(&cashu),
        Some(&PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"))
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_envelope_update_overwrites() {
    let mut setup = PrivateTestSetup::new().await;

    // First write: lightning only.
    let mut payment_endpoints_v1 = HashMap::new();
    payment_endpoints_v1.insert(
        PaymentEndpointIdentifier::new("bitcoin-lightning").unwrap(),
        PaymentEndpointPayload::new("v1"),
    );
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints_v1),
    )
    .await
    .unwrap();

    // Second write: completely different map (onchain only).
    let onchain = PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap();
    let mut payment_endpoints_v2 = HashMap::new();
    payment_endpoints_v2.insert(onchain.clone(), PaymentEndpointPayload::new("v2"));
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints_v2),
    )
    .await
    .unwrap();

    // The helper drains queued unread updates and returns the latest map.
    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.payment_endpoints.len(), 1);
    assert_eq!(
        received.payment_endpoints.get(&onchain),
        Some(&PaymentEndpointPayload::new("v2"))
    );

    // Backlog is drained, so a second immediate call returns empty.
    let empty = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap();
    assert!(empty.is_none());

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_envelope_rejects_oversized_payload() {
    let mut setup = PrivateTestSetup::new().await;

    // Build a map whose serialized JSON exceeds PUBKY_NOISE_MSG_LEN (1000 bytes).
    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let oversized_value = "x".repeat(1000);
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method, PaymentEndpointPayload::new(oversized_value));

    let result = set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
        "expected Validation error about size, got: {err}"
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_preserves_newer_receipt_access_messages() {
    let mut setup = PrivateTestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());

    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();
    send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("Private Payment Envelope should not be lost behind Receipt Access message");
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));
    let pending_kinds = setup.receiver_link.pending_private_message_kinds_for_test();
    assert_eq!(pending_kinds.len(), 1);
    assert_eq!(pending_kinds[0].as_str(), "paykit.receipt_access");

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_preserves_older_receipt_access_messages() {
    let mut setup = PrivateTestSetup::new().await;

    send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());

    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("Private Payment Envelope should be found without dropping Receipt Access message");
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));
    let pending_kinds = setup.receiver_link.pending_private_message_kinds_for_test();
    assert_eq!(pending_kinds.len(), 1);
    assert_eq!(pending_kinds[0].as_str(), "paykit.receipt_access");

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_drops_unknown_messages_without_buffering_them() {
    let mut setup = PrivateTestSetup::new().await;

    send_raw_private_message(
        &mut setup.sender_link,
        r#"{"version":1,"kind":"paykit.future_kind","payload":"ignored"}"#,
    )
    .await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("valid Private Payment Envelope should survive unknown earlier message");
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));
    assert_eq!(
        setup.receiver_link.pending_private_message_count_for_test(),
        0
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_ignores_malformed_messages_before_valid_payment() {
    let mut setup = PrivateTestSetup::new().await;

    send_raw_private_message(&mut setup.sender_link, "not-json").await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("valid Private Payment Envelope should survive malformed earlier message");
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));
    assert_eq!(
        setup.receiver_link.pending_private_message_count_for_test(),
        0
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_ignores_malformed_messages_after_valid_payment() {
    let mut setup = PrivateTestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let data = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(method.clone(), data.clone());
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints),
    )
    .await
    .unwrap();
    send_raw_private_message(&mut setup.sender_link, "not-json").await;

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("valid Private Payment Envelope should survive malformed later message");
    assert_eq!(received.payment_endpoints.get(&method), Some(&data));
    assert_eq!(
        setup.receiver_link.pending_private_message_count_for_test(),
        0
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn get_private_payment_envelope_keeps_latest_payment_without_dropping_other_kinds() {
    let mut setup = PrivateTestSetup::new().await;

    let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
    let mut payment_endpoints_v1 = HashMap::new();
    payment_endpoints_v1.insert(method.clone(), PaymentEndpointPayload::new("v1"));
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints_v1),
    )
    .await
    .unwrap();

    send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

    let mut payment_endpoints_v2 = HashMap::new();
    payment_endpoints_v2.insert(method.clone(), PaymentEndpointPayload::new("v2"));
    set_private_payment_envelope(
        &mut setup.sender_link,
        &private_payment_envelope(&payment_endpoints_v2),
    )
    .await
    .unwrap();

    let received = get_private_payment_envelope(&mut setup.receiver_link)
        .await
        .unwrap()
        .expect("latest Private Payment Envelope should be returned");
    assert_eq!(
        received.payment_endpoints.get(&method),
        Some(&PaymentEndpointPayload::new("v2"))
    );
    let pending_kinds = setup.receiver_link.pending_private_message_kinds_for_test();
    assert_eq!(pending_kinds.len(), 1);
    assert_eq!(pending_kinds[0].as_str(), "paykit.receipt_access");

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

// ── Parallel writer/reader happy-path test ──────────────────────────

/// Polls [`get_private_payment_envelope`] until a non-empty result is returned.
/// Panics on timeout (10 s).
async fn poll_private_payment_envelope(link: &mut EncryptedLink) -> PrivatePaymentEnvelope {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let timeout = Duration::from_secs(10);

    loop {
        assert!(
            start.elapsed() < timeout,
            "Private Payment Envelopes poll timed out after {timeout:?}"
        );

        if let Some(result) = get_private_payment_envelope(link).await.unwrap() {
            if !result.payment_endpoints.is_empty() {
                return result;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// End-to-end test that spins up a testnet and homeserver in the main
/// task, then exercises the private payment API from two concurrent
/// tasks (writer and reader) that perform a Noise XX handshake and
/// exchange encrypted payment data.
///
/// Coverage:
/// - Encrypted Link: initiate, accept, handshake (polling loops)
/// - Private Payment Envelopes: set, get (with polling)
/// - Link cleanup: close
/// - All interactions use only public `paykit_lib` functions
#[tokio::test]
async fn test_parallel_writer_reader_happy_path() {
    // ── Shared infrastructure (main task) ───────────────────────────

    let testnet = build_testnet().await;
    let homeserver = testnet.homeserver_app();

    // Writer (Alice): authenticated session + SDK for outbox reads.
    let writer_sdk = testnet.sdk().unwrap();
    let writer_keypair = Keypair::random();
    let writer_session = writer_sdk
        .signer(writer_keypair.clone())
        .signup(&homeserver.public_key(), None)
        .await
        .unwrap();
    let writer_pubkey = writer_session.info().public_key().clone();

    // Reader (Bob): authenticated session for the Encrypted Link
    // responder role + SDK for outbox reads.
    let reader_sdk = testnet.sdk().unwrap();
    let reader_keypair = Keypair::random();
    let reader_session = reader_sdk
        .signer(reader_keypair.clone())
        .signup(&homeserver.public_key(), None)
        .await
        .unwrap();
    let reader_pubkey = reader_session.info().public_key().clone();

    // ── Writer task ─────────────────────────────────────────────────

    let w_session = writer_session.clone();
    let w_reader_pubkey = reader_pubkey;

    let writer_handle = tokio::spawn(async move {
        // 1. Initiate Encrypted Link handshake.
        let handshake = initiate_encrypted_link(
            w_session.clone(),
            writer_keypair.secret_key(),
            &w_reader_pubkey,
            writer_sdk,
        )
        .unwrap();

        // 2. Drive handshake to completion (polling loop).
        let mut link = drive_handshake_to_completion(handshake).await;

        // 3. Send Private Payment Envelopes.
        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbcpriv..."),
        );
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap(),
            PaymentEndpointPayload::new("bc1priv..."),
        );
        set_private_payment_envelope(&mut link, &private_payment_envelope(&payment_endpoints))
            .await
            .unwrap();

        // 4. Clean up.
        close_encrypted_link(link).await.unwrap();
        w_session.signout().await.unwrap();
    });

    // ── Reader task ─────────────────────────────────────────────────

    let r_session = reader_session.clone();
    let r_writer_pubkey = writer_pubkey;

    let reader_handle = tokio::spawn(async move {
        // 1. Accept Encrypted Link handshake.
        let handshake = accept_encrypted_link(
            r_session.clone(),
            reader_keypair.secret_key(),
            &r_writer_pubkey,
            reader_sdk,
        )
        .unwrap();

        // 2. Drive handshake to completion (polling loop).
        let mut link = drive_handshake_to_completion(handshake).await;

        // 3. Poll for Private Payment Envelopes (writer may not have sent yet).
        let private = poll_private_payment_envelope(&mut link).await;
        assert_eq!(
            private.payment_endpoints.len(),
            2,
            "expected 2 Payment Endpoints, got {}",
            private.payment_endpoints.len()
        );
        assert_eq!(
            private
                .payment_endpoints
                .get(&PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap()),
            Some(&PaymentEndpointPayload::new("lnbcpriv...")),
        );
        assert_eq!(
            private
                .payment_endpoints
                .get(&PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap()),
            Some(&PaymentEndpointPayload::new("bc1priv...")),
        );

        // 4. Clean up.
        close_encrypted_link(link).await.unwrap();
        r_session.signout().await.unwrap();
    });

    // ── Join both tasks ─────────────────────────────────────────────

    let (writer_result, reader_result) = tokio::join!(writer_handle, reader_handle);
    writer_result.expect("writer task panicked");
    reader_result.expect("reader task panicked");

    // Testnet drops here, cleaning up the ephemeral homeserver.
}
