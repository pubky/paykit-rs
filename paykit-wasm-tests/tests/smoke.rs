//! THROWAWAY (#411): offline smoke tests for the browser (wasm32) SDK spike.
//!
//! Every test runs on both targets:
//! - native: `cargo test -p paykit-wasm-tests` (tokio current-thread runtime)
//! - wasm32: `cargo test -p paykit-wasm-tests --target wasm32-unknown-unknown`
//!   with `wasm-bindgen-test-runner` as the cargo runner. Node is the default;
//!   `WASM_BINDGEN_USE_BROWSER=1` runs the same binary in headless Chrome and
//!   `WASM_BINDGEN_USE_DEDICATED_WORKER=1` runs it in a dedicated worker.
//!
//! "Golden" values below were produced by the native build. The wasm build
//! must reproduce them byte for byte, which checks that the crypto, key
//! derivation and serialization paths behave the same in the browser.
//!
//! Nothing here leaves the machine. `test_auth_flow_url_offline` points the
//! auth relay at a closed loopback port, and
//! `test_bounded_fetch_read_over_loopback` fetches from a loopback server.

use std::{hash::BuildHasher, time::Duration};

// THROWAWAY (#411): on wasm32 Paykit's public maps use a getrandom-seeded
// hasher, so callers must build them with these aliases (see
// paykit_lib::collections). Natively they are the std types.
use paykit_lib::collections::{HashMap, RandomState as PaykitRandomState};

use paykit_lib::{
    derive_paykit_noise_public_key, derive_paykit_noise_secret_key,
    parse_payment_request_event_message, parse_private_payment_list_json, rt,
    serialize_payment_request_event, serialize_private_payment_list_json, PaykitAppId,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PrivateApplicationMessage,
    PrivatePaymentList,
};
use paykit_sdk::{
    export_backup_state, parse_pubky_auth_url, Clock, InMemoryStorage, PaykitSdk, PaykitSdkConfig,
    PaymentAdapter, PubkyIdentityCapability, PubkyLocalSecretKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySessionProvider, SystemClock, PAYKIT_SESSION_CAPABILITIES,
};
use sha2::{Digest, Sha256};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test;

/// Run each test as `#[wasm_bindgen_test]` on wasm32 and `#[tokio::test]` natively.
macro_rules! smoke_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
        async fn $name() $body
    };
}

/// BIP39 test vector (all-zero entropy). Public test data, not a real key.
const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                             abandon abandon abandon about";

// Golden values from the native build (see module docs).
const GOLDEN_PUBKY_PUBLIC_KEY: &str = "aihfhgdfshrj8nz9ofo7khayc1mgcqa4wrrdjahs5tmgo4pna3iy";
const GOLDEN_PAYKIT_NOISE_PUBLIC_KEY: &str =
    "pubkyfreop7wqmqyig5by1g5zyi6qbehk86mat5gnyq39fge4hxpoifgo";
const GOLDEN_NOISE_TRANSCRIPT_SHA256: &str =
    "a8fd2e0e5e7c74a70964dd79a91fdbc9f082b0f547841e59970e8e7905dba892";
const GOLDEN_ASYMMETRIC_PATHS_SHA256: &str =
    "9b754cdb70ac71c4bc75215a912c16d16d7d3a58e7a1840f2bdae59e0e14708b";

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

fn golden(name: &str, actual: &str, expected: &str) {
    // Print so a native run can (re)generate the constants above.
    println!("GOLDEN {name} = {actual}");
    assert_eq!(
        actual, expected,
        "golden value {name} differs on this target"
    );
}

fn app_id() -> PaykitAppId {
    PaykitAppId::new("wasm-smoke-app").unwrap()
}

// ---------------------------------------------------------------------------
// Randomness and clocks
// ---------------------------------------------------------------------------

smoke_test! {
    async fn test_randomness_sources_produce_distinct_nonzero_bytes() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        getrandom_03::fill(&mut a).unwrap();
        getrandom_03::fill(&mut b).unwrap();
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, b);

        // ed25519 key generation (pubky -> rand_core OsRng -> getrandom).
        let k1 = pubky::Keypair::random();
        let k2 = pubky::Keypair::random();
        assert_ne!(k1.public_key().to_string(), k2.public_key().to_string());

        // uuid v4 (Event IDs, Payment Request IDs).
        assert_ne!(uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    }
}

smoke_test! {
    async fn test_wall_clocks_do_not_panic() {
        let now = chrono::Utc::now();
        assert!(now.timestamp() > 1_767_225_600, "clock reads before 2026: {now}");
        let sdk_now = SystemClock.now();
        assert!((sdk_now - now).num_seconds().abs() < 5);
    }
}

// ---------------------------------------------------------------------------
// rt shim (timers + tasks)
// ---------------------------------------------------------------------------

smoke_test! {
    async fn test_rt_sleep_waits_and_instant_advances() {
        let start = rt::Instant::now();
        let wall_start = chrono::Utc::now();
        rt::sleep(Duration::from_millis(120)).await;
        let elapsed = start.elapsed();
        let wall_elapsed = (chrono::Utc::now() - wall_start).num_milliseconds();
        assert!(elapsed >= Duration::from_millis(120), "slept only {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "slept far too long: {elapsed:?}");
        assert!(wall_elapsed >= 110, "wall clock saw only {wall_elapsed} ms");
    }
}

smoke_test! {
    async fn test_rt_timeout_fires_and_ready_future_wins() {
        let pending = rt::timeout(
            Duration::from_millis(50),
            std::future::pending::<()>(),
        )
        .await;
        assert!(pending.is_err(), "timeout did not fire");

        let ready = rt::timeout(Duration::from_millis(50), async { 7u8 }).await;
        assert_eq!(ready.unwrap(), 7);

        // Already-expired deadline still lets a ready future win, as in tokio.
        let expired = rt::timeout_at(rt::Instant::now(), async { 9u8 }).await;
        assert_eq!(expired.unwrap(), 9);
    }
}

smoke_test! {
    async fn test_rt_spawn_join_and_abort() {
        let handle = rt::spawn(async {
            rt::sleep(Duration::from_millis(10)).await;
            41 + 1
        });
        assert_eq!(handle.await.unwrap(), 42);

        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in_task = ran.clone();
        let handle = rt::spawn(async move {
            rt::sleep(Duration::from_millis(200)).await;
            ran_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        rt::sleep(Duration::from_millis(10)).await;
        handle.abort();
        let err = handle.await.unwrap_err();
        assert!(err.is_cancelled());
        rt::sleep(Duration::from_millis(300)).await;
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "aborted task still ran");
    }
}

smoke_test! {
    async fn test_rt_interval_ticks_at_period() {
        let mut interval = rt::interval(Duration::from_millis(40));
        let start = rt::Instant::now();
        interval.tick().await; // first tick is immediate
        interval.tick().await;
        interval.tick().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(75), "3 ticks took only {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5));
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test]
async fn test_rt_lease_clock_is_monotonic_and_advances() {
    let first = rt::lease_clock().unwrap();
    rt::sleep(Duration::from_millis(100)).await;
    let second = rt::lease_clock().unwrap();
    assert!(
        second >= first + Duration::from_millis(95),
        "{first:?} -> {second:?}"
    );
}

// ---------------------------------------------------------------------------
// HashDoS hardening (paykit_lib::collections)
// ---------------------------------------------------------------------------

smoke_test! {
    async fn test_paykit_hashers_are_independently_keyed() {
        // Two fresh builders must hash the same key differently, and a map
        // built on them must behave like a normal HashMap.
        let probe = "pubky-peer-or-event-id";
        let first = PaykitRandomState::default();
        let second = PaykitRandomState::default();
        assert_ne!(first.hash_one(probe), second.hash_one(probe));
        assert_eq!(first.hash_one(probe), first.hash_one(probe));

        let mut map: HashMap<String, usize> = HashMap::default();
        for i in 0..1_000 {
            map.insert(format!("event-{i}"), i);
        }
        assert_eq!(map.len(), 1_000);
        assert_eq!(map.get("event-777"), Some(&777));
    }
}

/// std seeds `RandomState` from 32-bit linear-memory addresses on
/// wasm32-unknown-unknown, so the high halves of both keys are always zero.
/// Paykit's builder must carry real entropy there.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test]
async fn test_paykit_hasher_seed_is_not_address_derived() {
    for _ in 0..4 {
        assert!(PaykitRandomState::new().seed_has_high_bits());
    }
}

// ---------------------------------------------------------------------------
// Validation and serialization
// ---------------------------------------------------------------------------

smoke_test! {
    async fn test_payment_endpoint_identifier_validation() {
        for ok in ["btc-lightning-bolt11", "a", "x.y_z-1", &"a".repeat(64)] {
            assert!(PaymentEndpointIdentifier::new(ok).is_ok(), "{ok} rejected");
        }
        for bad in ["", ".", "..", "private", "a/b", "a b", "../x", &"a".repeat(65), "caf\u{e9}"] {
            assert!(PaymentEndpointIdentifier::new(bad).is_err(), "{bad:?} accepted");
        }
    }
}

smoke_test! {
    async fn test_private_payment_list_round_trip_and_size() {
        let mut endpoints = HashMap::default();
        endpoints.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbc1smoke"),
        );
        endpoints.insert(
            PaymentEndpointIdentifier::new("btc-onchain-p2wpkh").unwrap(),
            PaymentEndpointPayload::new("bc1qsmoke"),
        );
        let list = PrivatePaymentList::new(app_id(), endpoints);
        let json = serialize_private_payment_list_json(&list).unwrap();
        assert!(json.len() <= pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN);
        let parsed = parse_private_payment_list_json(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.app_id(), &app_id());
        assert_eq!(
            parsed
                .get(&PaymentEndpointIdentifier::new("btc-onchain-p2wpkh").unwrap())
                .unwrap()
                .as_str(),
            "bc1qsmoke"
        );

        assert!(parse_private_payment_list_json("{not json").is_err());
        let wrong_kind = json.replace("paykit.private_payment_list", "paykit.payment_request");
        assert!(parse_private_payment_list_json(&wrong_kind).is_err());
    }
}

fn payment_request_json() -> String {
    r#"{
        "version": 1,
        "kind": "paykit.payment_request",
        "app_id": "wasm-smoke-app",
        "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "request": {
            "amount": { "value": "0.001", "asset": "btc" },
            "payment_reference": "invoice-2026-0001",
            "proposal_expires_at": null,
            "recurrence": null,
            "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
            "required_app_id": null,
            "metadata": {}
        }
    }"#
    .to_string()
}

fn message(raw_json: String) -> PrivateApplicationMessage {
    let value: serde_json::Value = serde_json::from_str(&raw_json).unwrap_or_default();
    PrivateApplicationMessage {
        version: value["version"].as_u64().map(|v| v as u8),
        kind: value["kind"].as_str().map(str::to_owned),
        app_id: value["app_id"].as_str().map(str::to_owned),
        raw_json,
    }
}

smoke_test! {
    async fn test_payment_request_event_parse_and_reserialize() {
        let parsed = parse_payment_request_event_message(&message(payment_request_json()))
            .expect("known kind");
        let event = parsed.event.as_ref().expect("valid Payment Request");
        assert_eq!(parsed.app_id.as_ref(), Some(&app_id()));

        let reserialized = serialize_payment_request_event(&app_id(), event).unwrap();
        let reparsed = parse_payment_request_event_message(&message(reserialized)).unwrap();
        assert_eq!(reparsed.event.as_ref().unwrap(), event);

        // Failure path: a bad amount is reported as a validation error and the
        // error text does not echo the decrypted value.
        let tampered = payment_request_json().replace("\"0.001\"", "\"SENTINEL-not-a-number\"");
        let bad = parse_payment_request_event_message(&message(tampered)).unwrap();
        let err = bad.event.unwrap_err();
        assert!(!err.contains("SENTINEL"), "error echoes plaintext: {err}");
    }
}

// ---------------------------------------------------------------------------
// Key derivation and Noise
// ---------------------------------------------------------------------------

smoke_test! {
    async fn test_identity_and_noise_key_derivation_matches_native() {
        let local = PubkyLocalSecretKey::from_bip39_mnemonic(TEST_MNEMONIC).unwrap();
        golden(
            "PUBKY_PUBLIC_KEY",
            local.public_key().as_str(),
            GOLDEN_PUBKY_PUBLIC_KEY,
        );
        let identity_secret = local.derive_paykit_identity_secret_key(paykit_lib::INITIAL_PAYKIT_KEY_GENERATION).unwrap();
        let noise_public = derive_paykit_noise_public_key(identity_secret.as_bytes());
        golden(
            "PAYKIT_NOISE_PUBLIC_KEY",
            &noise_public.to_string(),
            GOLDEN_PAYKIT_NOISE_PUBLIC_KEY,
        );
        assert!(PubkyLocalSecretKey::from_bip39_mnemonic("abandon abandon").is_err());
    }
}

fn noise_builder(seed: [u8; 32]) -> snow::Builder<'static> {
    // Same protocol name and resolver as pubky-noise's DataLinkContext.
    snow::Builder::with_resolver(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap(),
        pubky_noise::snow_crypto_resolver::ReplayResolver::new(seed),
    )
}

smoke_test! {
    async fn test_noise_xx_handshake_and_transport() {
        let alice_static = derive_paykit_noise_secret_key(&[1u8; 32]);
        let bob_static = derive_paykit_noise_secret_key(&[2u8; 32]);
        let mut alice = noise_builder([3u8; 32])
            .local_private_key(&alice_static)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut bob = noise_builder([4u8; 32])
            .local_private_key(&bob_static)
            .unwrap()
            .build_responder()
            .unwrap();

        let mut transcript: Vec<Vec<u8>> = Vec::new();
        let mut buf = vec![0u8; 65535];
        let mut out = vec![0u8; 65535];
        // -> e
        let n = alice.write_message(&[], &mut buf).unwrap();
        transcript.push(buf[..n].to_vec());
        bob.read_message(&buf[..n], &mut out).unwrap();
        // <- e, ee, s, es
        let n = bob.write_message(&[], &mut buf).unwrap();
        transcript.push(buf[..n].to_vec());
        alice.read_message(&buf[..n], &mut out).unwrap();
        // -> s, se
        let n = alice.write_message(&[], &mut buf).unwrap();
        transcript.push(buf[..n].to_vec());
        bob.read_message(&buf[..n], &mut out).unwrap();
        assert!(alice.is_handshake_finished() && bob.is_handshake_finished());
        assert_eq!(alice.get_handshake_hash(), bob.get_handshake_hash());
        assert_eq!(
            alice.get_remote_static().unwrap(),
            pubky_noise::derive_static_public_key(&bob_static)
        );
        assert_eq!(
            bob.get_remote_static().unwrap(),
            pubky_noise::derive_static_public_key(&alice_static)
        );

        let mut alice = alice.into_transport_mode().unwrap();
        let mut bob = bob.into_transport_mode().unwrap();
        let plaintext = serialize_private_payment_list_json(&PrivatePaymentList::new(
            app_id(),
            HashMap::default(),
        ))
        .unwrap();
        let n = alice.write_message(plaintext.as_bytes(), &mut buf).unwrap();
        let ciphertext = buf[..n].to_vec();
        transcript.push(ciphertext.clone());
        let m = bob.read_message(&ciphertext, &mut out).unwrap();
        assert_eq!(&out[..m], plaintext.as_bytes());

        // Tampering is detected (Poly1305 tag).
        let n = bob.write_message(b"reply", &mut buf).unwrap();
        let mut tampered = buf[..n].to_vec();
        tampered[0] ^= 1;
        assert!(alice.read_message(&tampered, &mut out).is_err());

        let parts: Vec<&[u8]> = transcript.iter().map(Vec::as_slice).collect();
        golden(
            "NOISE_TRANSCRIPT_SHA256",
            &sha256_hex(&parts),
            GOLDEN_NOISE_TRANSCRIPT_SHA256,
        );
    }
}

smoke_test! {
    async fn test_asymmetric_path_derivation_is_symmetric_and_matches_native() {
        let alice = pubky::Keypair::from_secret(&[5u8; 32]);
        let bob = pubky::Keypair::from_secret(&[6u8; 32]);
        let base = "/pub/paykit/v0/private/";
        let (alice_write, alice_read) = pubky_noise::path_derivation::derive_asymmetric_paths(
            &alice.secret(),
            &bob.public_key(),
            b"paykit-wasm-smoke",
            base,
        );
        let (bob_write, bob_read) = pubky_noise::path_derivation::derive_asymmetric_paths(
            &bob.secret(),
            &alice.public_key(),
            b"paykit-wasm-smoke",
            base,
        );
        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert_ne!(alice_write, alice_read);
        golden(
            "ASYMMETRIC_PATHS_SHA256",
            &sha256_hex(&[alice_write.as_bytes(), alice_read.as_bytes()]),
            GOLDEN_ASYMMETRIC_PATHS_SHA256,
        );
    }
}

// ---------------------------------------------------------------------------
// SDK runtime without a session
// ---------------------------------------------------------------------------

struct NoSessionProvider;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl PubkySessionProvider for NoSessionProvider {
    async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
        Ok(None)
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        Ok(None)
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        Ok(())
    }
}

struct NoPayments;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl PaymentAdapter for NoPayments {}

smoke_test! {
    async fn test_sdk_initialize_signed_out_with_in_memory_storage() {
        let storage = InMemoryStorage::new();
        let sdk = PaykitSdk::new(
            storage.clone(),
            NoSessionProvider,
            NoPayments,
            PaykitSdkConfig::new("wasm-smoke-app").unwrap(),
        );
        let status = sdk.initialize().await.unwrap();
        assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
        assert!(status.public_key.is_none());
        let again = sdk.identity_status().await.unwrap().unwrap();
        assert_eq!(again.capability, PubkyIdentityCapability::SignedOut);

        let backup = export_backup_state(&storage).await.unwrap();
        assert_eq!(backup.version, paykit_sdk::SDK_BACKUP_VERSION);
        assert!(!backup.content_fingerprint().unwrap().is_empty());
    }
}

smoke_test! {
    async fn test_auth_flow_url_offline() {
        // Relay on a closed loopback port: the background poller fails
        // locally, nothing leaves the machine.
        let bootstrap = PubkySessionBootstrap::new("paykit-wasm-smoke.example")
            .unwrap()
            .with_auth_relay("http://127.0.0.1:1/")
            .unwrap();
        let request = bootstrap
            .start_sign_in_auth(PAYKIT_SESSION_CAPABILITIES)
            .await
            .unwrap();
        let details = parse_pubky_auth_url(request.authorization_url()).unwrap();
        assert_eq!(details.capabilities, PAYKIT_SESSION_CAPABILITIES);
        assert_eq!(details.client_id, "paykit-wasm-smoke.example");
        assert!(details.relay_url.starts_with("http://127.0.0.1:1/"));
        assert!(parse_pubky_auth_url("https://not-an-auth-url").is_err());
        drop(request);
    }
}

// ---------------------------------------------------------------------------
// fetch + bounded read through paykit_lib::ResponseChunks
// ---------------------------------------------------------------------------

/// Same pattern as Paykit's bounded reads: stop as soon as the limit is crossed.
#[cfg(target_arch = "wasm32")]
async fn read_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut chunks = paykit_lib::ResponseChunks::new(response);
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next_chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > limit {
            return Err("limit exceeded".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

// reqwest only accepts http(s) URLs (a `data:` URL fails with BadScheme), so
// the body comes from a loopback server: under Node a one-shot `node:http`
// server on 127.0.0.1:0, in a browser page or worker the test runner's own
// same-origin document. Only wasm32: it exercises the browser `fetch` backend.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export async function smoke_body_url() {
  const isNode = typeof process !== 'undefined' && process.versions && process.versions.node
    && typeof window === 'undefined' && typeof WorkerGlobalScope === 'undefined';
  if (!isNode) {
    return self.location.href;
  }
  const http = await import('node:http');
  const body = 'x'.repeat(4096);
  let served = 0;
  const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain', 'connection': 'close' });
    res.end(body);
    served += 1;
    if (served >= 2) server.close();
  });
  server.unref();
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return `http://127.0.0.1:${server.address().port}/`;
}
"#)]
extern "C" {
    async fn smoke_body_url() -> wasm_bindgen::JsValue;
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test]
async fn test_bounded_fetch_read_over_loopback() {
    let url = smoke_body_url().await.as_string().expect("url string");
    let client = reqwest::Client::new();
    let response = client.get(&url).send().await.expect("fetch loopback URL");
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    let body = read_bounded(response, 1 << 20).await.unwrap();
    assert!(body.len() > 100, "body only {} bytes", body.len());

    // Same body, limit one byte short: the bounded reader must refuse it.
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(
        read_bounded(response, body.len() - 1).await.unwrap_err(),
        "limit exceeded"
    );
}
