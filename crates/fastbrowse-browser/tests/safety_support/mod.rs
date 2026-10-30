//! Helpers the safety and speed suites share: `observe_until`, a fixture server whose pages a test writes as a closure, and a CDP proxy that
//! logs every message, standing in for the Python tests' wrapped `send_raw` and `_evaluate`.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use fastbrowse_browser::page::CdpPage;
use fastbrowse_core::page::Observation;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::{Instant, sleep};
use tokio_tungstenite::tungstenite::Message;

pub fn has(observation: &Observation, label: &str) -> bool {
    observation.controls.iter().any(|c| c.label.contains(label))
}

/// `common::observe_until`, except that a failed observe is retried until the deadline.
pub async fn observe_until(page: &CdpPage, label: &str) -> Observation {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(observation) = page.observe().await
            && has(&observation, label)
        {
            return observation;
        }
        assert!(Instant::now() < deadline, "condition was not met in time");
        sleep(Duration::from_millis(50)).await;
    }
}

// -- a fixture server --------------------------------------------------------------------------------------------

pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    /// Held before the response is written, as the Python handlers' `time.sleep`.
    pub delay: Duration,
}

impl Reply {
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::of("text/html", body)
    }

    pub fn of(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, content_type, headers: vec![], body: body.into(), delay: Duration::ZERO }
    }
}

pub type Handler = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

fn answer(stream: TcpStream, handler: &Handler) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    // Chrome preconnects (two sockets for a meta refresh) and closes them unused. As `BaseHTTPRequestHandler` does,
    // a connection that sends no request line is not a request, so the handler, which may count requests, never sees it.
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let reply = handler(&path);
    thread::sleep(reply.delay);
    let reason = match reply.status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nConnection: close\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = stream;
    // The next navigation can discard a slow response before it is written.
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&reply.body));
}

/// Serve `handler` on 127.0.0.1, on `listener` if given, returning the port.
pub fn serve_on(listener: TcpListener, handler: Handler) -> u16 {
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            thread::spawn(move || answer(stream, &handler));
        }
    });
    port
}

/// `quote`: percent-encode everything but unreserved characters and `/`, as `urllib.parse.quote` does.
pub fn quote(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The query parameter `name`, percent-decoded, as `parse_qs(query)[name][0]`.
pub fn query_param(path: &str, name: &str) -> String {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=')
            && key == name
        {
            return unquote(value);
        }
    }
    String::new()
}

fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// -- a CDP proxy that logs ---------------------------------------------------------------------------------------

/// One message through the proxy: `sent` is the client's command, otherwise the browser's event or reply.
#[derive(Debug, Clone)]
pub struct Logged {
    pub sent: bool,
    pub message: Value,
}

pub struct CdpProxy {
    pub url: String,
    pub log: Arc<Mutex<Vec<Logged>>>,
}

impl CdpProxy {
    /// Commands sent with `method`, from the log's `start`.
    pub fn sent_since(&self, start: usize, method: &str) -> Vec<Value> {
        self.log.lock().unwrap()[start..]
            .iter()
            .filter(|l| l.sent && l.message["method"] == method)
            .map(|l| l.message.clone())
            .collect()
    }

    /// Events the browser sent with `method`, from the log's `start`.
    pub fn events_since(&self, start: usize, method: &str) -> Vec<Value> {
        self.log.lock().unwrap()[start..]
            .iter()
            .filter(|l| !l.sent && l.message["method"] == method)
            .map(|l| l.message.clone())
            .collect()
    }

    /// The page's own evaluations (`CdpPage::evaluate`, which alone asks for a promise's value by value) on
    /// any session, from the log's `start`: what the Python tests count by wrapping `page._evaluate`.
    pub fn page_evaluations_since(&self, start: usize) -> usize {
        self.sent_since(start, "Runtime.evaluate")
            .iter()
            .filter(|m| m["params"]["awaitPromise"] == true && m["params"]["returnByValue"] == true)
            .count()
    }

    pub fn len(&self) -> usize {
        self.log.lock().unwrap().len()
    }
}

/// A websocket proxy in front of `upstream` that logs every text message both ways and changes none.
pub async fn cdp_proxy(upstream: &str) -> CdpProxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let path = upstream.splitn(4, '/').nth(3).unwrap_or("").to_owned();
    let log: Arc<Mutex<Vec<Logged>>> = Arc::default();
    let (upstream, kept) = (upstream.to_owned(), log.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (upstream, log) = (upstream.clone(), kept.clone());
            tokio::spawn(async move {
                let Ok(client) = tokio_tungstenite::accept_async(stream).await else { return };
                let Ok((browser, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else { return };
                let (mut client_tx, mut client_rx) = client.split();
                let (mut browser_tx, mut browser_rx) = browser.split();
                let record = |log: &Mutex<Vec<Logged>>, sent: bool, message: &Message| {
                    if let Message::Text(text) = message
                        && let Ok(message) = serde_json::from_str::<Value>(text.as_str())
                    {
                        log.lock().unwrap().push(Logged { sent, message });
                    }
                };
                let outbound = {
                    let log = log.clone();
                    async move {
                        while let Some(Ok(message)) = client_rx.next().await {
                            record(&log, true, &message);
                            if browser_tx.send(message).await.is_err() {
                                break;
                            }
                        }
                        let _ = browser_tx.close().await;
                    }
                };
                let inbound = async move {
                    while let Some(Ok(message)) = browser_rx.next().await {
                        record(&log, false, &message);
                        if client_tx.send(message).await.is_err() {
                            break;
                        }
                    }
                    let _ = client_tx.close().await;
                };
                tokio::join!(outbound, inbound);
            });
        }
    });
    CdpProxy { url: format!("ws://127.0.0.1:{port}/{path}"), log }
}
