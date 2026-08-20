use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::{filter::Targets, layer::SubscriberExt as _, registry, Layer as _};

use super::*;

struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("trace buffer lock poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_recovery_marker_fetch_tracing_redacts_derived_address() {
    let mut builder = pubky::PubkyHttpClient::builder();
    builder
        .pkarr(|pkarr| {
            pkarr
                .no_default_network()
                .relays(&["http://127.0.0.1:1"])
                .unwrap()
        })
        .request_timeout(Duration::from_millis(100));
    let storage = pubky::Pubky::with_client(builder.build().unwrap()).public_storage();
    let local_keypair = Keypair::random();
    let remote_identity_keypair = Keypair::random();
    let remote_noise_keypair = Keypair::random();
    let (_, read_path) = encrypted_link_recovery_marker_paths(
        &local_keypair.secret_key(),
        &remote_noise_keypair.public_key(),
    );
    let address = format!("{}{}", remote_identity_keypair.public_key(), read_path);
    let trace_bytes = Arc::new(Mutex::new(Vec::new()));
    let writer_bytes = Arc::clone(&trace_bytes);
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(move || TraceWriter(Arc::clone(&writer_bytes)))
        .with_filter(Targets::new().with_target("paykit_lib", tracing::Level::TRACE));
    let subscriber = registry().with(layer);

    let result = fetch_encrypted_link_recovery_marker(
        &storage,
        &local_keypair.secret_key(),
        &remote_identity_keypair.public_key(),
        &remote_noise_keypair.public_key(),
    )
    .with_subscriber(subscriber)
    .await;

    assert!(matches!(result, Err(PaykitError::Transport { .. })));
    let traces = String::from_utf8(
        trace_bytes
            .lock()
            .expect("trace buffer lock poisoned")
            .clone(),
    )
    .unwrap();
    assert!(traces.contains("fetching text resource"));
    assert!(!traces.contains(&read_path));
    assert!(!traces.contains(&address));
    assert!(!traces.contains("127.0.0.1:1"));
    assert!(!traces.contains("Connection refused"));
}
