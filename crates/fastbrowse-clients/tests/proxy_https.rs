//! A proxy with an HTTPS URL receives a TLS handshake before CONNECT can be sent.

use std::time::Duration;

use fastbrowse_clients::http::{HyperTransport, Method, Request, Transport};
use hyper_util::client::proxy::matcher::Matcher;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn an_https_proxy_is_contacted_using_tls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = HyperTransport::with_proxy(
        Matcher::builder().https(format!("https://{}", listener.local_addr().unwrap())).build(),
    );
    let proxy = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.read_u8().await.unwrap()
        })
        .await
        .ok()
    });
    let result = transport
        .send(Request {
            method: Method::Get,
            url: "https://model.example/v1/models".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(5),
            max_bytes: None,
            handoff: Default::default(),
        })
        .await;
    // The proxy double closes after the first byte: certificate validation is not part of this check.
    assert_eq!(proxy.await.unwrap(), Some(0x16), "an HTTPS proxy must receive a TLS handshake; request: {result:?}");
}
