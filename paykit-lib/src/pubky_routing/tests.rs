use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};

use super::*;

async fn response_for_test(wire: &'static str) -> reqwest::Response {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        stream.write_all(wire.as_bytes()).unwrap();
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    server.join().unwrap();
    response
}

#[tokio::test]
async fn test_bounded_body_accepts_chunked_body_at_limit() {
    let response = response_for_test(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(response.content_length(), None);

    assert_eq!(
        read_bounded_body(response, 4, "test resource")
            .await
            .unwrap(),
        b"abcd"
    );
}

#[tokio::test]
async fn test_bounded_body_rejects_chunked_body_over_limit() {
    let response = response_for_test(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(response.content_length(), None);

    let error = read_bounded_body(response, 4, "test resource")
        .await
        .unwrap_err();

    assert!(is_response_size_limit_exceeded(&error));
}

#[tokio::test]
async fn test_bounded_body_rejects_oversized_content_length_before_reading() {
    let response =
        response_for_test("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
            .await;

    let error = read_bounded_body(response, 4, "test resource")
        .await
        .unwrap_err();

    assert!(is_response_size_limit_exceeded(&error));
}

#[tokio::test]
async fn test_payment_list_byte_budget_keeps_consumption_after_body_failure() {
    let mut remaining = 8;
    let mut response = response_for_test(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    ).await;
    let error = read_response_body_with_budget(&mut response, "endpoint", 4, &mut remaining)
        .await
        .unwrap_err();
    assert!(is_response_size_limit_exceeded(&error));
    assert_eq!(remaining, 3);

    let mut response = response_for_test(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nfghi\r\n0\r\n\r\n",
    ).await;
    assert!(
        read_response_body_with_budget(&mut response, "endpoint", remaining, &mut remaining)
            .await
            .is_err()
    );
    assert_eq!(remaining, 0);

    let mut remaining = 8;
    let mut response = response_for_test(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n4\r\n",
    ).await;
    let error = read_response_body_with_budget(&mut response, "endpoint", 8, &mut remaining)
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitError::Transport { .. }));
    assert_eq!(remaining, 5);
}

#[test]
fn test_payment_list_request_budget_stops_at_zero() {
    let mut remaining = 1;
    consume_payment_list_request(&mut remaining).unwrap();
    assert_eq!(remaining, 0);
    let error = consume_payment_list_request(&mut remaining).unwrap_err();
    assert!(is_payment_list_limit_exceeded(&error));
    assert_eq!(remaining, 0);
}
