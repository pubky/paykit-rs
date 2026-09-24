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
