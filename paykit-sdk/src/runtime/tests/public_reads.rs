use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_single_response(response: &'static [u8]) -> reqwest::Response {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        let _ = socket.write_all(response).await;
        let _ = socket.shutdown().await;
    });
    reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn test_read_public_response_rejects_declared_oversize_body() {
    let resp = spawn_single_response(b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\n\r\nAAAA").await;
    let err = read_public_response(resp, 8, "test fetch")
        .await
        .expect_err("declared body over the limit must be rejected");
    assert!(matches!(err, PaykitSdkError::Protocol { .. }));
}

#[tokio::test]
async fn test_read_public_response_accepts_body_at_limit() {
    let resp = spawn_single_response(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nAAAAAAAA").await;
    let bytes = read_public_response(resp, 8, "test fetch")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"AAAAAAAA");
}

#[tokio::test]
async fn test_read_public_response_drops_oversized_response_before_eof() {
    for raw in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n8\r\nAAAAAAAA\r\n8\r\nAAAAAAAA\r\n",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(raw).await.unwrap();
            // Keep the response incomplete until the reader drops the connection.
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        });
        let resp = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let err = read_public_response(resp, 8, "test fetch")
                .await
                .expect_err("oversized response must be rejected before EOF");
            assert!(matches!(err, PaykitSdkError::Protocol { .. }));
            server.await.unwrap();
        })
        .await
        .expect("bounded read and connection shutdown must finish");
    }
}

#[tokio::test]
async fn test_read_public_response_accepts_streamed_and_empty_bodies() {
    for (raw, limit, expected) in [
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n2\r\n34\r\n0\r\n\r\n"
                .as_slice(),
            4,
            b"1234".as_slice(),
        ),
        (
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n1234",
            4,
            b"1234",
        ),
        (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", 0, b""),
    ] {
        let resp = spawn_single_response(raw).await;
        assert_eq!(
            read_public_response(resp, limit, "test fetch")
                .await
                .unwrap()
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn test_read_public_response_reports_detectable_truncation() {
    for raw in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n12".as_slice(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n",
    ] {
        let resp = spawn_single_response(raw).await;
        assert!(matches!(
            read_public_response(resp, 4, "test fetch").await,
            Err(PaykitSdkError::Transport { .. })
        ));
    }
}

#[tokio::test]
async fn test_read_public_response_discards_error_and_missing_bodies() {
    let response = spawn_single_response(
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 999999\r\n\r\nsentinel-private-error",
    ).await;
    let error = read_public_response(response, 0, "test fetch")
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Transport { .. }));
    assert!(error.to_string().contains("HTTP 500"));
    assert!(!error.to_string().contains("sentinel-private-error"));

    for raw in [
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 999999\r\n\r\n".as_slice(),
        b"HTTP/1.1 410 Gone\r\nContent-Length: 999999\r\n\r\n",
    ] {
        let response = spawn_single_response(raw).await;
        assert_eq!(
            read_public_response(response, 0, "test fetch")
                .await
                .unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn test_read_public_response_zero_rejects_nonempty_stream() {
    let response = spawn_single_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n",
    )
    .await;
    assert!(matches!(
        read_public_response(response, 0, "test fetch").await,
        Err(PaykitSdkError::Protocol { .. })
    ));
}
