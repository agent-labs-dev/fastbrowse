//! The HTTP seam every client sends through: what Python's shared `httpx.AsyncClient` is to its clients.
//!
//! A [`Transport`] sends one request and reads its whole body, as `httpx.AsyncClient.post` does. The production one
//! is hyper over rustls; tests hand in a closure, as Python's tests hand httpx a `MockTransport`. [`Http`] is the
//! handle a run shares between its clients: the transport, and the trace its retries and spans are written to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::py::json::{Style, write};
use fastbrowse_core::telemetry::{Trace, monotonic};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// In the order they were given; names as the client wrote them.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Stop reading the body once it passes this many bytes, as a streamed `httpx` read can: the response then holds
    /// one byte more than the bound, so a caller sees it was passed without the rest landing in memory.
    pub max_bytes: Option<usize>,
    /// The longest wait for the response head, and then for each body chunk, as an httpx timeout bounds each read. A
    /// body that keeps arriving is read to its end, however long a model takes to write it: a provider that is still
    /// working sends keep-alive bytes, so silence this long is a stall.
    pub timeout: Duration,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The bearer token the request carried, kept so an error quoting the body can blank it, as Python reads it back
    /// from `response.request`.
    pub(crate) credential: String,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response { status, headers: Vec::new(), body: body.into(), credential: String::new() }
    }

    pub fn json(status: u16, body: &Value) -> Response {
        Response::new(status, write(body, Style::HTTPX)).with_header("content-type", "application/json")
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    /// Replace the request credential in provider text before it reaches traces or errors.
    pub(crate) fn scrub(&self, text: &str) -> String {
        scrub(text, &self.credential)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `response.text`: the body decoded as UTF-8, a byte it cannot decode replaced.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `response.json()`, or `None` where Python's would raise.
    pub fn json_value(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub(crate) fn scrub(text: &str, credential: &str) -> String {
    if credential.is_empty() { text.to_string() } else { text.replace(credential, "[api key]") }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// A request that produced no response, named by the httpx exception Python would have caught, since the name is what
/// a failure reports: its text can quote the request, credential included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// `httpx.TimeoutException`, or [`Request::timeout`] passed in silence: a repeat can clear it.
    Timeout(&'static str),
    /// `httpx.NetworkError`, `RemoteProtocolError`, `ProxyError` or `DecodingError`: a repeat can clear it.
    Transient(&'static str),
    /// Any other `httpx.HTTPError` (an unsupported scheme, an invalid URL) fails the same way every time.
    Fatal(&'static str),
}

impl TransportError {
    pub fn name(&self) -> &'static str {
        match self {
            TransportError::Timeout(name) | TransportError::Transient(name) | TransportError::Fatal(name) => name,
        }
    }
}

pub trait Transport: Send + Sync {
    /// Send `request` and read its whole body.
    fn send(&self, request: Request) -> BoxFuture<'static, Result<Response, TransportError>>;
}

impl<F> Transport for F
where
    F: Fn(Request) -> BoxFuture<'static, Result<Response, TransportError>> + Send + Sync,
{
    fn send(&self, request: Request) -> BoxFuture<'static, Result<Response, TransportError>> {
        self(request)
    }
}

/// The `http` event for one POST: every model request and reply in full, so a trace shows what a model was asked and
/// what it said. Headers carry the API keys and are left out. A POST whose future is dropped in flight is written as
/// `cancelled` rather than missing.
struct HttpEvent<'a> {
    trace: &'a Trace,
    url: &'a str,
    body: &'a Value,
    began: f64,
    written: bool,
}

impl HttpEvent<'_> {
    fn write(&mut self, outcome: impl FnOnce(&mut Map<String, Value>)) {
        self.written = true;
        self.trace.event("http", || {
            let mut fields = Map::new();
            fields.insert("url".into(), self.url.into());
            fields.insert("request".into(), traced(self.body));
            fields.insert("began".into(), self.began.into());
            fields.insert("ended".into(), monotonic().into());
            outcome(&mut fields);
            fields
        });
    }
}

#[cfg(test)]
mod tests {
    use hyper_util::client::proxy::matcher::Matcher;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{HyperTransport, Method, Request, Transport, TransportError, traced};

    /// A proxy that answers the first request it reads with a refusal and returns that request's first line.
    async fn refusing_proxy() -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = vec![0; 4096];
            let read = stream.read(&mut head).await.unwrap();
            stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&head[..read]).lines().next().unwrap_or_default().to_string()
        });
        (url, seen)
    }

    fn get(url: &str) -> Request {
        Request {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: std::time::Duration::from_secs(5),
            max_bytes: None,
        }
    }

    #[tokio::test]
    async fn an_https_request_is_tunnelled_through_the_proxy_the_environment_names() {
        let (proxy, seen) = refusing_proxy().await;
        let transport = HyperTransport::with_proxy(Matcher::builder().https(proxy).build());
        let error = transport.send(get("https://openrouter.ai/api/v1/chat/completions")).await.unwrap_err();
        assert_eq!(seen.await.unwrap(), "CONNECT openrouter.ai:443 HTTP/1.1");
        assert!(matches!(error, TransportError::Transient("ConnectError")), "{error:?}");
    }

    #[tokio::test]
    async fn a_host_no_proxy_exempts_is_reached_directly() {
        let (proxy, seen) = refusing_proxy().await;
        let transport = HyperTransport::with_proxy(Matcher::builder().https(proxy).no("127.0.0.1").build());
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", origin.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut head = vec![0; 4096];
            let _ = stream.read(&mut head).await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi").await.unwrap();
        });
        let response = transport.send(get(&url)).await.unwrap();
        served.await.unwrap();
        seen.abort();
        assert_eq!((response.status, response.body.as_slice()), (200, b"hi".as_slice()));
    }

    #[test]
    fn an_image_sent_to_jev_is_kept_by_name_and_not_by_its_bytes() {
        let body = json!({
            "model": "jev-1.14",
            "state": {"viewport_screenshot": "screenshot-abc"},
            "assets": {"screenshot-abc": {"type": "image", "format": "png", "data_base64": "iVBORw0KGgo"}},
        });
        let traced = traced(&body);
        assert_eq!(traced["assets"]["screenshot-abc"]["data_base64"], "<image>");
        assert_eq!(traced["assets"]["screenshot-abc"]["type"], "image");
        assert_eq!(traced["state"]["viewport_screenshot"], "screenshot-abc");
        assert_eq!(traced["model"], "jev-1.14");
    }

    #[test]
    fn an_images_data_url_is_kept_as_one_image() {
        let body = json!({"messages": [{"content": [{"type": "image_url",
            "image_url": {"url": "data:image/png;base64,iVBORw0KGgo"}}]}]});
        assert_eq!(traced(&body)["messages"][0]["content"][0]["image_url"]["url"], "data:image/<image>");
    }
}

impl Drop for HttpEvent<'_> {
    fn drop(&mut self) {
        if !self.written {
            self.write(|fields| {
                fields.insert("error".into(), "cancelled".into());
            });
        }
    }
}

/// The request body as the trace keeps it. An image is its own bulk and is already named by its pixels' digest, since a
/// screenshot's asset name is `screenshot-<sha256>`, so the name stands in for the bytes: otherwise a run that sends a
/// viewport screenshot on every step of a drawn page writes a 102 MB trace (BU Bench bu2-157, 51 steps), a record no
/// tool can read.
fn traced(body: &Value) -> Value {
    let mut body = body.clone();
    blank_images(&mut body);
    body
}

/// A Jev image asset's `data_base64`, and a data URL's payload, replaced as the eval replay already replaces them.
fn blank_images(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image") && map.contains_key("data_base64") {
                map.insert("data_base64".into(), Value::String("<image>".into()));
            }
            for item in map.values_mut() {
                blank_images(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                blank_images(item);
            }
        }
        Value::String(text) if text.starts_with("data:image/") => *text = "data:image/<image>".into(),
        _ => {}
    }
}

/// What a run's clients share: one connection pool, one trace.
#[derive(Clone)]
pub struct Http {
    transport: Arc<dyn Transport>,
    pub trace: Trace,
    /// The header a provider takes its key in, when that is none of the usual three.
    credential_header: Option<&'static str>,
}

impl Http {
    pub fn new(transport: Arc<dyn Transport>) -> Http {
        Http { transport, trace: Trace::disabled(), credential_header: None }
    }

    /// hyper over rustls with the Mozilla roots.
    pub fn hyper() -> Http {
        Http::new(Arc::new(HyperTransport::new()))
    }

    pub fn with_trace(mut self, trace: Trace) -> Http {
        self.trace = trace;
        self
    }

    pub(crate) fn with_credential_header(mut self, header: Option<&'static str>) -> Http {
        self.credential_header = header;
        self
    }

    /// `http.post(url, json=body, headers=headers, timeout=seconds)`: `seconds` bounds each silence, see
    /// [`Request::timeout`].
    pub async fn post_json(
        &self,
        url: &str,
        body: &Value,
        headers: &[(String, String)],
        seconds: f64,
    ) -> Result<Response, TransportError> {
        let mut all = headers.to_vec();
        all.push(("content-type".into(), "application/json".into()));
        // httpx writes `json=` compact and UTF-8, which `Style::HTTPX` is.
        let request = Request {
            method: Method::Post,
            url: url.to_string(),
            headers: all,
            body: write(body, Style::HTTPX).into(),
            max_bytes: None,
            timeout: if seconds == f64::INFINITY { Duration::MAX } else { Duration::from_secs_f64(seconds) },
        };
        let mut event = HttpEvent { trace: &self.trace, url, body, began: monotonic(), written: false };
        let result = self.send(request).await;
        event.write(|fields| match &result {
            Ok(response) => {
                fields.insert("status".into(), response.status.into());
                let text = response.text();
                let text = response.scrub(&text);
                fields.insert("response".into(), serde_json::from_str(&text).unwrap_or(Value::String(text)));
            }
            Err(error) => {
                fields.insert("error".into(), error.name().into());
            }
        });
        result
    }

    pub async fn get(&self, url: &str, headers: &[(String, String)], seconds: f64) -> Result<Response, TransportError> {
        self.get_bounded(url, headers, seconds, None).await
    }

    /// [`Http::get`] reading at most one byte past `max_bytes` of the body; see [`Request::max_bytes`].
    pub async fn get_bounded(
        &self,
        url: &str,
        headers: &[(String, String)],
        seconds: f64,
        max_bytes: Option<usize>,
    ) -> Result<Response, TransportError> {
        let request = Request {
            method: Method::Get,
            url: url.to_string(),
            headers: headers.to_vec(),
            body: Vec::new(),
            max_bytes,
            timeout: Duration::from_secs_f64(seconds),
        };
        self.send(request).await
    }

    async fn send(&self, request: Request) -> Result<Response, TransportError> {
        // Anthropic takes its key as `x-api-key` and Azure as `api-key`; an error that echoes either is scrubbed too,
        // as is one that echoes the header a custom endpoint was configured with.
        let credential = request
            .header("authorization")
            .map(|value| value.strip_prefix("Bearer ").unwrap_or(value))
            .or_else(|| request.header("x-api-key"))
            .or_else(|| request.header("api-key"))
            .or_else(|| self.credential_header.and_then(|name| request.header(name)))
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let mut response = self.transport.send(request).await?;
        response.credential = credential;
        Ok(response)
    }
}

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;
type TunnelClient = Client<HttpsConnector<Tunnel<HttpConnector>>, Full<Bytes>>;

/// The production transport: HTTP/1.1 with a keep-alive pool, TLS through rustls. A destination the environment's
/// `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` covers (and `NO_PROXY` does not exempt) goes through that proxy with an
/// HTTP CONNECT tunnel, as httpx does by default; TLS runs inside the tunnel, so the proxy sees only the host.
pub struct HyperTransport {
    client: HyperClient,
    proxy: Matcher,
    /// One keep-alive pool per proxy, built when a request first needs it.
    tunnels: Mutex<HashMap<String, TunnelClient>>,
}

impl Default for HyperTransport {
    fn default() -> Self {
        HyperTransport::new()
    }
}

impl HyperTransport {
    pub fn new() -> HyperTransport {
        HyperTransport::with_proxy(Matcher::from_env())
    }

    pub fn with_proxy(proxy: Matcher) -> HyperTransport {
        let connector =
            hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots().https_or_http().enable_http1().build();
        HyperTransport {
            client: Client::builder(TokioExecutor::new()).build(connector),
            proxy,
            tunnels: Mutex::default(),
        }
    }

    fn tunnel_client(&self, proxy: &hyper::Uri) -> TunnelClient {
        let mut tunnels = self.tunnels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        tunnels
            .entry(proxy.to_string())
            .or_insert_with(|| {
                let tunnel = Tunnel::new(proxy.clone(), HttpConnector::new());
                let connector = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_or_http()
                    .enable_http1()
                    .wrap_connector(tunnel);
                Client::builder(TokioExecutor::new()).build(connector)
            })
            .clone()
    }
}

impl Transport for HyperTransport {
    fn send(&self, request: Request) -> BoxFuture<'static, Result<Response, TransportError>> {
        let client = self.client.clone();
        let tunnel = self
            .proxy
            .intercept(&hyper::Uri::from_maybe_shared(request.url.clone()).unwrap_or_default())
            .map(|proxy| self.tunnel_client(proxy.uri()));
        Box::pin(async move {
            let uri: hyper::Uri = request.url.parse().map_err(|_| TransportError::Fatal("InvalidURL"))?;
            if !matches!(uri.scheme_str(), Some("http" | "https")) {
                return Err(TransportError::Fatal("UnsupportedProtocol"));
            }
            let method = match request.method {
                Method::Get => hyper::Method::GET,
                Method::Post => hyper::Method::POST,
            };
            let (request_max, silence) = (request.max_bytes, request.timeout);
            let stalled = |_| TransportError::Timeout("TimeoutError");
            let mut builder = hyper::Request::builder().method(method).uri(uri);
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let outgoing =
                builder.body(Full::new(Bytes::from(request.body))).map_err(|_| TransportError::Fatal("InvalidURL"))?;
            let sent = async {
                match &tunnel {
                    Some(tunnel) => tunnel.request(outgoing).await,
                    None => client.request(outgoing).await,
                }
            };
            let response = tokio::time::timeout(silence, sent).await.map_err(stalled)?.map_err(|error| {
                if error.is_connect() {
                    TransportError::Transient("ConnectError")
                } else {
                    TransportError::Transient("RemoteProtocolError")
                }
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned())
                })
                .collect();
            let mut incoming = response.into_body();
            let mut body = Vec::new();
            while let Some(frame) = tokio::time::timeout(silence, incoming.frame()).await.map_err(stalled)? {
                let frame = frame.map_err(|_| TransportError::Transient("ReadError"))?;
                if let Some(chunk) = frame.data_ref() {
                    body.extend_from_slice(chunk);
                }
                if let Some(max) = request_max
                    && body.len() > max
                {
                    body.truncate(max + 1);
                    break;
                }
            }
            Ok(Response { status, headers, body, credential: String::new() })
        })
    }
}
