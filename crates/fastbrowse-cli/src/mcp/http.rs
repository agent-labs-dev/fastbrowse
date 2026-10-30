//! MCP's streamable HTTP transport at `/mcp`, as the `mcp` SDK serves it under uvicorn, behind the bearer token.
//!
//! Stateful sessions: `initialize` opens one and names it in `mcp-session-id`, and every later request carries it. A
//! request is answered on its own event stream, with the progress and log notifications it causes ahead of its
//! response; a notification gets 202. Bound to loopback, the Host and Origin headers are held to loopback too, the
//! SDK's DNS rebinding protection.

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use fastbrowse_core::py::json::{self, Style};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{HEALTH_PATH, McpServer, Outbox, Session};

/// A response body: one buffer, or the events of a stream as they come.
pub enum Payload {
    Full(Option<Bytes>),
    Events(mpsc::UnboundedReceiver<Bytes>),
}

impl Body for Payload {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Payload::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Payload::Events(events) => events.poll_recv(context).map(|event| event.map(|bytes| Ok(Frame::data(bytes)))),
        }
    }

    // A known length is written as `content-length`, as uvicorn writes it; an event stream is chunked.
    fn size_hint(&self) -> hyper::body::SizeHint {
        match self {
            Payload::Full(bytes) => hyper::body::SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            Payload::Events(_) => hyper::body::SizeHint::default(),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Payload::Full(None))
    }
}

type Reply = Response<Payload>;

fn reply(status: StatusCode, content_type: Option<&str>, body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(Payload::Full(Some(body.into())));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response.headers_mut().insert("content-type", HeaderValue::from_str(content_type).expect("a header value"));
    }
    response
}

/// `_create_error_response`: a JSON-RPC error with the id `server-error`, and the session's id when there is one.
fn server_error(status: StatusCode, code: i64, message: &str, session: Option<&str>) -> Reply {
    let body = json!({"jsonrpc": "2.0", "id": "server-error", "error": {"code": code, "message": message}});
    let mut response = reply(status, Some("application/json"), json::write(&body, Style::PYDANTIC));
    with_session(&mut response, session);
    response
}

fn with_session(response: &mut Reply, session: Option<&str>) {
    if let Some(value) = session.and_then(|session| HeaderValue::from_str(session).ok()) {
        response.headers_mut().insert("mcp-session-id", value);
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A new session's id: 32 hex digits, as `uuid4().hex` writes one.
fn session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let seed = format!("{nanos}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    fastbrowse_core::py::sha256_hex(seed.as_bytes())[..32].to_string()
}

pub struct Http {
    server: Arc<McpServer>,
    token: Option<String>,
    /// DNS rebinding protection, on when the server is bound to loopback.
    loopback_only: bool,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// Constant-time comparison, so the token cannot be recovered a byte at a time from response timings.
fn same(supplied: &[u8], expected: &[u8]) -> bool {
    supplied.len() == expected.len() && supplied.iter().zip(expected).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn allowed_host(host: &str) -> bool {
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|base| host.strip_prefix(base).is_some_and(|rest| rest.starts_with(':')))
}

fn allowed_origin(origin: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|base| origin.strip_prefix(base).is_some_and(|rest| rest.starts_with(':')))
}

fn event(message: &Value) -> Bytes {
    Bytes::from(format!("event: message\r\ndata: {}\r\n\r\n", json::write(message, Style::PYDANTIC)))
}

fn events(receiver: mpsc::UnboundedReceiver<Bytes>, session: &str) -> Reply {
    let mut response = Response::new(Payload::Events(receiver));
    let headers = response.headers_mut();
    headers.insert("cache-control", HeaderValue::from_static("no-cache, no-transform"));
    headers.insert("connection", HeaderValue::from_static("keep-alive"));
    headers.insert("content-type", HeaderValue::from_static("text/event-stream"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    with_session(&mut response, Some(session));
    response
}

impl Http {
    pub fn new(server: Arc<McpServer>, host: &str) -> Arc<Http> {
        let token = server.config.mcp_token.clone();
        let loopback_only = matches!(host, "127.0.0.1" | "localhost" | "::1");
        Arc::new(Http { server, token, loopback_only, sessions: Mutex::default() })
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Result<Reply, Infallible> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_string();
        if let Some(token) = &self.token
            && path != HEALTH_PATH
        {
            let supplied = parts.headers.get("authorization").map_or(&b""[..], HeaderValue::as_bytes);
            if !same(supplied, format!("Bearer {token}").as_bytes()) {
                let mut refused =
                    reply(StatusCode::UNAUTHORIZED, Some("text/plain; charset=utf-8"), "missing or wrong bearer token");
                refused.headers_mut().insert("www-authenticate", HeaderValue::from_static("Bearer"));
                return Ok(refused);
            }
        }
        if path == HEALTH_PATH {
            return Ok(if parts.method == Method::GET {
                reply(StatusCode::OK, Some("application/json"), r#"{"status":"ok"}"#)
            } else {
                reply(StatusCode::METHOD_NOT_ALLOWED, Some("text/plain; charset=utf-8"), "Method Not Allowed")
            });
        }
        if path != "/mcp" {
            return Ok(reply(StatusCode::NOT_FOUND, Some("text/plain; charset=utf-8"), "Not Found"));
        }
        let body = body.collect().await.map(|collected| collected.to_bytes()).unwrap_or_default();
        Ok(self.mcp(&parts.method, &parts.headers, &body))
    }

    fn mcp(&self, method: &Method, headers: &HeaderMap, body: &[u8]) -> Reply {
        let asked = header(headers, "mcp-session-id").map(str::to_string);
        let (id, opening) = match &asked {
            Some(id) if self.sessions().contains_key(id) => (id.clone(), false),
            Some(_) => return server_error(StatusCode::NOT_FOUND, -32600, "Session not found", None),
            None => (session_id(), true),
        };
        let session = if opening { Arc::new(Session::default()) } else { self.sessions()[&id].clone() };
        let (response, established) = self.transport(method, headers, body, &id, &session, opening);
        if opening && established {
            self.sessions().insert(id, session);
        }
        response
    }

    /// `StreamableHTTPServerTransport.handle_request`. The flag says whether an opening request opened the session.
    fn transport(
        &self,
        method: &Method,
        headers: &HeaderMap,
        body: &[u8],
        id: &str,
        session: &Arc<Session>,
        opening: bool,
    ) -> (Reply, bool) {
        let refused = |response: Reply| (response, false);
        if *method == Method::POST
            && !header(headers, "content-type")
                .is_some_and(|value| value.to_lowercase().starts_with("application/json"))
        {
            crate::log::warning("Missing Content-Type header in POST request");
            return refused(reply(StatusCode::BAD_REQUEST, None, "Invalid Content-Type header"));
        }
        if self.loopback_only {
            if !header(headers, "host").is_some_and(allowed_host) {
                return refused(reply(StatusCode::MISDIRECTED_REQUEST, None, "Invalid Host header"));
            }
            if header(headers, "origin").is_some_and(|origin| !allowed_origin(origin)) {
                return refused(reply(StatusCode::FORBIDDEN, None, "Invalid Origin header"));
            }
        }
        let accept: Vec<String> = header(headers, "accept")
            .unwrap_or("")
            .split(',')
            .map(|part| part.split(';').next().unwrap_or("").trim().to_lowercase())
            .collect();
        let (json_ok, sse_ok) = (
            accept.iter().any(|part| part == "application/json" || part == "application/*" || part == "*/*"),
            accept.iter().any(|part| part == "text/event-stream" || part == "text/*" || part == "*/*"),
        );
        let check_headers = || -> Option<Reply> {
            if opening {
                return Some(server_error(
                    StatusCode::BAD_REQUEST,
                    -32600,
                    "Bad Request: Missing session ID",
                    Some(id),
                ));
            }
            match header(headers, "mcp-protocol-version") {
                Some(version) if !super::SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => {
                    let message = format!(
                        "Bad Request: Unsupported protocol version: {version}. Supported versions: {}",
                        super::SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                    );
                    Some(server_error(StatusCode::BAD_REQUEST, -32600, &message, Some(id)))
                }
                _ => None,
            }
        };
        match *method {
            Method::POST => {
                if !(json_ok && sse_ok) {
                    let message = "Not Acceptable: Client must accept both application/json and text/event-stream";
                    return refused(server_error(StatusCode::NOT_ACCEPTABLE, -32600, message, Some(id)));
                }
                let message: Value = match serde_json::from_slice(body) {
                    Ok(message) => message,
                    Err(error) => {
                        let message = format!("Parse error: {error}");
                        return refused(server_error(StatusCode::BAD_REQUEST, -32700, &message, Some(id)));
                    }
                };
                let Value::Object(object) = &message else {
                    let message = "Validation error: a JSON-RPC message is one JSON object";
                    return refused(server_error(StatusCode::BAD_REQUEST, -32602, message, Some(id)));
                };
                let is_request = object.contains_key("id") && object.contains_key("method");
                let initializing = is_request && object.get("method").and_then(Value::as_str) == Some("initialize");
                if !initializing && let Some(response) = check_headers() {
                    return refused(response);
                }
                let line = json::write(&message, Style::PYDANTIC).into_bytes();
                if !is_request {
                    let silent: Outbox = Arc::new(|_| {});
                    self.server.receive(session, &line, &silent);
                    let mut accepted = reply(StatusCode::ACCEPTED, Some("application/json"), Bytes::new());
                    with_session(&mut accepted, Some(id));
                    return (accepted, false);
                }
                let (sender, receiver) = mpsc::unbounded_channel();
                let request_id = object.get("id").cloned().unwrap_or(Value::Null);
                // The stream holds what this request causes, and closes once its response is written.
                let stream = Arc::new(Mutex::new(Some(sender)));
                let outbox: Outbox = Arc::new(move |message: Value| {
                    let mut stream = stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let ends = message.get("id") == Some(&request_id) && message.get("method").is_none();
                    if let Some(sender) = stream.as_ref() {
                        let _ = sender.send(event(&message));
                    }
                    if ends {
                        stream.take();
                    }
                });
                self.server.receive(session, &line, &outbox);
                (events(receiver, id), initializing)
            }
            Method::DELETE => {
                if let Some(response) = check_headers() {
                    return refused(response);
                }
                self.sessions().remove(id);
                let mut done = reply(StatusCode::OK, Some("application/json"), Bytes::new());
                with_session(&mut done, Some(id));
                (done, false)
            }
            _ => refused(server_error(StatusCode::METHOD_NOT_ALLOWED, -32600, "Method Not Allowed", Some(id))),
        }
    }
}

/// Serve until the process is stopped.
pub async fn serve(server: Arc<McpServer>, host: &str, port: i64) -> std::io::Result<()> {
    let port = u16::try_from(port).map_err(|_| std::io::Error::other(format!("port {port} is out of range")))?;
    let bind = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("fastbrowse-mcp: serving on http://{host}:{port}/mcp");
    serve_on(listener, server, host).await
}

/// Serve on a socket already bound, as `host` names the address it was bound to.
pub async fn serve_on(listener: tokio::net::TcpListener, server: Arc<McpServer>, host: &str) -> std::io::Result<()> {
    let http = Http::new(server, host);
    loop {
        let (stream, _) = listener.accept().await?;
        let http = http.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| http.clone().handle(request));
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
        });
    }
}
