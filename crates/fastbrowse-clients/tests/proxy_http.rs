//! An HTTP destination without an explicit port keeps HTTP's default port through a proxy.

use std::time::Duration;

use fastbrowse_clients::http::{HyperTransport, Method, Request, Transport};
use hyper_util::client::proxy::matcher::Matcher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn an_http_origin_without_a_port_is_reached_on_port_80() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = HyperTransport::with_proxy(
        Matcher::builder().http(format!("http://{}", listener.local_addr().unwrap())).build(),
    );
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        String::from_utf8(head).unwrap()
    });
    let _ = transport
        .send(Request {
            method: Method::Get,
            url: "http://model.example/v1/models".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(5),
            max_bytes: None,
            handoff: Default::default(),
        })
        .await;
    let head = tokio::time::timeout(Duration::from_secs(5), proxy).await.unwrap().unwrap();
    let first = head.lines().next().unwrap();
    assert!(
        first == "CONNECT model.example:80 HTTP/1.1"
            || first == "GET http://model.example/v1/models HTTP/1.1"
            || first == "GET http://model.example:80/v1/models HTTP/1.1",
        "the proxy must target HTTP port 80 (by tunnel or forwarding), got {first}"
    );
}

#[tokio::test]
async fn an_http_origin_works_with_a_forward_proxy_that_does_not_allow_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = HyperTransport::with_proxy(
        Matcher::builder().http(format!("http://{}", listener.local_addr().unwrap())).build(),
    );
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        let response = if head.starts_with(b"GET http://model.example:80/v1/models HTTP/1.1\r\n")
            || head.starts_with(b"GET http://model.example/v1/models HTTP/1.1\r\n")
        {
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".as_slice()
        } else {
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".as_slice()
        };
        stream.write_all(response).await.unwrap();
        String::from_utf8(head).unwrap()
    });
    let result = transport
        .send(Request {
            method: Method::Get,
            url: "http://model.example:80/v1/models".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(5),
            max_bytes: None,
            handoff: Default::default(),
        })
        .await;
    let head = tokio::time::timeout(Duration::from_secs(5), proxy).await.unwrap().unwrap();
    assert_eq!(result.as_ref().map(|response| response.status), Ok(200), "HTTP forwarding failed: {head}");
}
