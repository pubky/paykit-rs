use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{default_pubky_client_config, session::pubky_client_builder};

#[tokio::test]
async fn test_pubky_publication_uses_relay_without_waiting_for_udp() {
    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("http://{}", relay.local_addr().unwrap());
    let reject_publication = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_relay(relay, reject_publication.clone()));
    let config = crate::FfiPubkyClientConfig {
        local_testnet_host: Some("127.0.0.1".into()),
        ..default_pubky_client_config()
    };

    for reject in [false, true] {
        reject_publication.store(reject, Ordering::SeqCst);
        let mut builder = pubky_client_builder(&config).unwrap();
        builder.pkarr(|pkarr| pkarr.relays(&[&relay_url]).unwrap());
        let client = pubky::Pubky::with_client(builder.build().unwrap());
        let signer = client.signer(pubky::Keypair::random());
        let homeserver = pubky::Keypair::random().public_key();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            signer.pkdns().publish_homeserver_force(Some(&homeserver)),
        )
        .await
        .expect("publication must complete using the HTTP relay");

        assert_eq!(
            result.is_err(),
            reject,
            "relay failures must reach the caller"
        );
        builder.pkarr(|pkarr| pkarr.no_relays());
        assert!(builder.build().is_err(), "direct DHT must remain disabled");
    }
    server.abort();
}

async fn serve_relay(listener: TcpListener, reject_publication: Arc<AtomicBool>) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let size = stream.read(&mut buffer).await.unwrap();
            assert_ne!(size, 0, "relay request ended before its body");
            request.extend_from_slice(&buffer[..size]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + content_length {
                    break;
                }
            }
        }
        let status = if request.starts_with(b"PUT ") {
            if reject_publication.load(Ordering::SeqCst) {
                "503 Service Unavailable"
            } else {
                "204 No Content"
            }
        } else {
            "404 Not Found"
        };
        stream
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
    }
}
