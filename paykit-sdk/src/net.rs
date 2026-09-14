//! Bounded reads for homeserver responses.

use crate::{PaykitSdkError, Result};

/// Read a public response body with a hard size bound.
///
/// A hostile homeserver can stream an arbitrarily large body, so the limit is
/// enforced while reading rather than after buffering. `Content-Length` is
/// checked first as a cheap early rejection when present.
pub(crate) async fn read_bounded_body(
    mut resp: reqwest::Response,
    max_bytes: usize,
    context: &'static str,
) -> Result<Vec<u8>> {
    if let Some(content_length) = resp.content_length() {
        if content_length > max_bytes as u64 {
            return Err(resource_too_large(context, max_bytes));
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|err| PaykitSdkError::Transport {
            context: context.into(),
            source: Some(err.into()),
        })?
    {
        if body.len() + chunk.len() > max_bytes {
            return Err(resource_too_large(context, max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn resource_too_large(context: &'static str, max_bytes: usize) -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context: format!("{context}: resource exceeds maximum size of {max_bytes} bytes"),
        source: None,
    }
}

#[cfg(test)]
mod tests {
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
    async fn test_read_bounded_body_rejects_declared_oversize_body() {
        let resp =
            spawn_single_response(b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\n\r\nAAAA").await;
        let err = read_bounded_body(resp, 8, "test fetch")
            .await
            .expect_err("declared body over the limit must be rejected");
        assert!(matches!(err, PaykitSdkError::Protocol { .. }));
    }

    #[tokio::test]
    async fn test_read_bounded_body_accepts_body_at_limit() {
        let resp =
            spawn_single_response(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nAAAAAAAA").await;
        let bytes = read_bounded_body(resp, 8, "test fetch").await.unwrap();
        assert_eq!(bytes, b"AAAAAAAA");
    }

    #[tokio::test]
    async fn test_read_bounded_body_drops_oversized_response_before_eof() {
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
                let err = read_bounded_body(resp, 8, "test fetch")
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
    async fn test_read_bounded_body_accepts_streamed_and_empty_bodies() {
        for (raw, limit, expected) in [
            (b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n2\r\n34\r\n0\r\n\r\n".as_slice(), 4, b"1234".as_slice()),
            (b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n1234", 4, b"1234"),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", 0, b""),
        ] {
            let resp = spawn_single_response(raw).await;
            assert_eq!(read_bounded_body(resp, limit, "test fetch").await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn test_read_bounded_body_reports_detectable_truncation() {
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n12".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n",
        ] {
            let resp = spawn_single_response(raw).await;
            assert!(matches!(
                read_bounded_body(resp, 4, "test fetch").await,
                Err(PaykitSdkError::Transport { .. })
            ));
        }
    }
}
