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
    async fn test_read_bounded_body_rejects_streamed_body_over_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            // Chunked transfer encoding with no Content-Length, streaming
            // forever. A reader that buffers the body first would never
            // terminate; the bounded reader must reject it while reading.
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            loop {
                if socket.write_all(b"8\r\nAAAAAAAA\r\n").await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_bounded_body(resp, 8, "test fetch"),
        )
        .await
        .expect("bounded read must terminate")
        .expect_err("a streamed body over the limit must be rejected");
        assert!(
            matches!(err, PaykitSdkError::Protocol { .. }),
            "expected a size-limit error, got: {err:?}"
        );
    }
}
