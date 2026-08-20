use super::*;

#[tokio::test]
async fn private_payment_list_empty_stream_returns_empty() {
    let mut setup = PrivateTestSetup::new().await;

    let messages = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert!(
        messages.is_empty(),
        "fresh link with no messages should return no payload"
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_list_round_trip() {
    let mut setup = PrivateTestSetup::new().await;

    let endpoint_identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let payload = PaymentEndpointPayload::new("lnbc1...");
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(endpoint_identifier.clone(), payload.clone());

    set_private_payment_list(
        &mut setup.sender_link,
        &PrivatePaymentList::new(test_app_id(), payment_endpoints),
    )
    .await
    .unwrap();

    let received = receive_latest_private_payment_list_for_test(&mut setup.receiver_link)
        .await
        .unwrap();
    assert_eq!(received.payment_endpoints.len(), 1);
    assert_eq!(received.app_id(), &test_app_id());
    assert_eq!(
        received.payment_endpoints.get(&endpoint_identifier),
        Some(&payload)
    );

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_list_multiple_endpoints() {
    let mut setup = PrivateTestSetup::new().await;

    let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
    let cashu = PaymentEndpointIdentifier::new("cashu-mint_id").unwrap();

    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(lightning.clone(), PaymentEndpointPayload::new("ln..."));
    payment_endpoints.insert(onchain.clone(), PaymentEndpointPayload::new("bc1p..."));
    payment_endpoints.insert(
        cashu.clone(),
        PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"),
    );

    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints),
    )
    .await
    .unwrap();

    let received = receive_latest_private_payment_list_for_test(&mut setup.receiver_link)
        .await
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
async fn private_payment_list_stream_exposes_multiple_updates() {
    let mut setup = PrivateTestSetup::new().await;

    let mut payment_endpoints_v1 = HashMap::new();
    payment_endpoints_v1.insert(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("v1"),
    );
    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints_v1),
    )
    .await
    .unwrap();

    let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
    let mut payment_endpoints_v2 = HashMap::new();
    payment_endpoints_v2.insert(onchain.clone(), PaymentEndpointPayload::new("v2"));
    set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints_v2),
    )
    .await
    .unwrap();

    let messages = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages
        .iter()
        .all(|message| { message.known_kind() == Some(PrivateMessageKind::PrivatePaymentList) }));

    let received = messages
        .into_iter()
        .filter_map(|message| parse_private_payment_list_json(&message.raw_json).ok())
        .next_back()
        .unwrap();
    assert_eq!(received.payment_endpoints.len(), 1);
    assert_eq!(
        received.payment_endpoints.get(&onchain),
        Some(&PaymentEndpointPayload::new("v2"))
    );

    let empty = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    assert!(empty.is_empty());

    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}

#[tokio::test]
async fn private_payment_list_rejects_oversized_payload() {
    let mut setup = PrivateTestSetup::new().await;

    let endpoint_identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let oversized_value = "x".repeat(1000);
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert(
        endpoint_identifier,
        PaymentEndpointPayload::new(oversized_value),
    );

    let result = set_private_payment_list(
        &mut setup.sender_link,
        &private_payment_list(&payment_endpoints),
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

async fn poll_private_payment_list(link: &mut EncryptedLink) -> PrivatePaymentList {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let timeout = Duration::from_secs(10);

    loop {
        assert!(
            start.elapsed() < timeout,
            "Private Payment Lists poll timed out after {timeout:?}"
        );

        if let Some(result) = receive_latest_private_payment_list_for_test(link).await {
            if !result.payment_endpoints.is_empty() {
                return result;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn test_parallel_writer_reader_happy_path() {
    let testnet = build_testnet().await;
    let homeserver = testnet.homeserver_app();

    let writer_sdk = testnet.sdk().unwrap();
    let writer_keypair = Keypair::random();
    let writer_session = writer_sdk
        .signer(writer_keypair.clone())
        .signup(&homeserver.public_key(), None)
        .await
        .unwrap();
    let writer_pubkey = writer_session.info().public_key().clone();

    let reader_sdk = testnet.sdk().unwrap();
    let reader_keypair = Keypair::random();
    let reader_session = reader_sdk
        .signer(reader_keypair.clone())
        .signup(&homeserver.public_key(), None)
        .await
        .unwrap();
    let reader_pubkey = reader_session.info().public_key().clone();

    let w_session = writer_session.clone();
    let w_reader_pubkey = reader_pubkey;
    let writer_noise_secret_key = derive_paykit_noise_secret_key(&writer_keypair.secret_key());
    let reader_noise_public_key = derive_paykit_noise_public_key(&reader_keypair.secret_key());

    let writer_handle = tokio::spawn(async move {
        let handshake = initiate_encrypted_link(
            w_session.clone(),
            writer_noise_secret_key,
            &w_reader_pubkey,
            &reader_noise_public_key,
            writer_sdk,
        )
        .unwrap();

        let mut link = drive_handshake_to_completion(handshake).await;

        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbcpriv..."),
        );
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap(),
            PaymentEndpointPayload::new("bc1priv..."),
        );
        set_private_payment_list(&mut link, &private_payment_list(&payment_endpoints))
            .await
            .unwrap();

        close_encrypted_link(link).await.unwrap();
        w_session.signout().await.unwrap();
    });

    let r_session = reader_session.clone();
    let r_writer_pubkey = writer_pubkey;
    let reader_noise_secret_key = derive_paykit_noise_secret_key(&reader_keypair.secret_key());
    let writer_noise_public_key = derive_paykit_noise_public_key(&writer_keypair.secret_key());

    let reader_handle = tokio::spawn(async move {
        let handshake = accept_encrypted_link(
            r_session.clone(),
            reader_noise_secret_key,
            &r_writer_pubkey,
            &writer_noise_public_key,
            reader_sdk,
        )
        .unwrap();

        let mut link = drive_handshake_to_completion(handshake).await;

        let private = poll_private_payment_list(&mut link).await;
        assert_eq!(
            private.payment_endpoints.len(),
            2,
            "expected 2 Payment Endpoints, got {}",
            private.payment_endpoints.len()
        );
        assert_eq!(
            private
                .payment_endpoints
                .get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
            Some(&PaymentEndpointPayload::new("lnbcpriv...")),
        );
        assert_eq!(
            private
                .payment_endpoints
                .get(&PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap()),
            Some(&PaymentEndpointPayload::new("bc1priv...")),
        );

        close_encrypted_link(link).await.unwrap();
        r_session.signout().await.unwrap();
    });

    let (writer_result, reader_result) = tokio::join!(writer_handle, reader_handle);
    writer_result.expect("writer task panicked");
    reader_result.expect("reader task panicked");
}
