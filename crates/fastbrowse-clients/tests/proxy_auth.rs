//! A proxy URL's credentials authorize CONNECT, including distinct identities at the same proxy address.

use std::time::Duration;

use fastbrowse_clients::http::{HyperTransport, Method, Request, Transport};
use hyper_util::client::proxy::matcher::Matcher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn proxy_url_credentials_are_sent_on_each_matching_tunnel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            requests.push(String::from_utf8(head).unwrap());
        }
        requests
    });
    let transport = HyperTransport::with_proxy(
        Matcher::builder()
            .https(format!("http://alice:secret@{address}"))
            .http(format!("http://bob:secret@{address}"))
            .build(),
    );
    for url in ["https://model.example/v1/models", "http://model.example:8080/v1/models"] {
        let _ = transport
            .send(Request {
                method: Method::Get,
                url: url.into(),
                headers: Vec::new(),
                body: Vec::new(),
                timeout: Duration::from_secs(5),
                max_bytes: None,
            })
            .await;
    }
    let requests = tokio::time::timeout(Duration::from_secs(5), proxy).await.unwrap().unwrap();
    for (head, expected) in requests.iter().zip(["Basic YWxpY2U6c2VjcmV0", "Basic Ym9iOnNlY3JldA=="]) {
        let auth = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("proxy-authorization").then_some(value.trim())
        });
        assert_eq!(auth, Some(expected), "CONNECT must carry the credentials from its matching proxy URL: {head}");
    }
}
