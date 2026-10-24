//! A minimal HTTP/1.1 client: one request per connection, for Browser Use Cloud's API and Chrome's
//! `/json/version`.
//!
//! It stands in for httpx and urllib, which the Python adapters used. Both callers send a handful of requests per
//! run, so a connection pool, redirects and compression would be weight the binary carries for nothing. TLS runs on
//! the rustls and ring stack the CDP websocket already links.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header named `name`, which HTTP compares without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorKind {
    /// The URL or a header could not be sent: a caller's mistake, not the network's.
    InvalidRequest,
    Timeout,
    Connect,
    Tls,
    /// The connection broke while the request or response was in flight.
    Io,
    /// The server answered with something other than HTTP/1.1.
    Protocol,
}

/// A failed request. It names the failure and never the request: headers carry API keys, and a URL can carry a
/// credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub kind: HttpErrorKind,
    detail: String,
}

impl HttpError {
    fn new(kind: HttpErrorKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into() }
    }

    /// httpx's `TRANSIENT_TRANSPORT`: a failure a repeat can clear. Only a request that could not be sent is not.
    pub fn is_transient(&self) -> bool {
        self.kind != HttpErrorKind::InvalidRequest
    }

    /// The httpx class name the Python messages quote.
    pub fn name(&self) -> &'static str {
        match self.kind {
            HttpErrorKind::InvalidRequest => "InvalidURL",
            HttpErrorKind::Timeout => "TimeoutException",
            HttpErrorKind::Connect | HttpErrorKind::Tls => "ConnectError",
            HttpErrorKind::Io => "NetworkError",
            HttpErrorKind::Protocol => "RemoteProtocolError",
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() { f.write_str(self.name()) } else { write!(f, "{}: {}", self.name(), self.detail) }
    }
}

impl std::error::Error for HttpError {}

struct Target {
    tls: bool,
    /// The host as a socket and TLS name take it: an IPv6 address without its brackets.
    host: String,
    port: u16,
    /// The `Host` header: the authority as written, minus credentials.
    authority: String,
    path: String,
}

fn parse_url(url: &str) -> Result<Target, HttpError> {
    let invalid = || HttpError::new(HttpErrorKind::InvalidRequest, "");
    let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        _ => return Err(invalid()),
    };
    let rest = rest.split_once('#').map_or(rest, |(rest, _)| rest);
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']').ok_or_else(invalid)?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port.filter(|port| !port.is_empty()) {
        Some(port) => port.parse().map_err(|_| invalid())?,
        None if tls => 443,
        None => 80,
    };
    if host.is_empty() || authority.chars().any(|c| c.is_ascii_control() || c == ' ') {
        return Err(invalid());
    }
    let path = match path {
        "" => "/".to_owned(),
        query if query.starts_with('?') => format!("/{query}"),
        path => path.to_owned(),
    };
    if path.chars().any(|c| c.is_ascii_control() || c == ' ') {
        return Err(invalid());
    }
    Ok(Target { tls, host: host.to_owned(), port, authority: authority.to_owned(), path })
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config =
                rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                    .with_safe_default_protocol_versions()
                    .expect("ring offers TLS 1.2 and 1.3")
                    .with_root_certificates(roots)
                    .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Send one request and read the whole response, within `timeout` in all.
///
/// `json` is sent as written, with `Content-Type: application/json`; callers serialize it themselves so its keys
/// keep their order. Every request asks the server to close the connection, which is how a body without a length
/// ends.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    json: Option<&str>,
    timeout: Duration,
) -> Result<Response, HttpError> {
    let target = parse_url(url)?;
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\nAccept-Encoding: identity\r\n\
         User-Agent: fastbrowse\r\n",
        target.path, target.authority
    );
    if let Some(json) = json {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", json.len()));
    }
    for (name, value) in headers {
        // A newline in a header would start another header, or a second request.
        if format!("{name}{value}").chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
            return Err(HttpError::new(HttpErrorKind::InvalidRequest, "a header holds a line break"));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut message = head.into_bytes();
    message.extend_from_slice(json.unwrap_or("").as_bytes());

    let exchange = async {
        let tcp = TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|error| HttpError::new(HttpErrorKind::Connect, error.to_string()))?;
        let _ = tcp.set_nodelay(true);
        if !target.tls {
            return exchange(tcp, &message).await;
        }
        let name = ServerName::try_from(target.host.clone())
            .map_err(|_| HttpError::new(HttpErrorKind::InvalidRequest, "the host is not a TLS server name"))?;
        let tls = tokio_rustls::TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await
            .map_err(|error| HttpError::new(HttpErrorKind::Tls, error.to_string()))?;
        exchange(tls, &message).await
    };
    tokio::time::timeout(timeout, exchange).await.unwrap_or_else(|_| Err(HttpError::new(HttpErrorKind::Timeout, "")))
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, message: &[u8]) -> Result<Response, HttpError> {
    stream.write_all(message).await.map_err(io_error)?;
    stream.flush().await.map_err(io_error)?;
    let mut buffer = Vec::new();
    loop {
        let (head_len, status, headers) = loop {
            if let Some(head) = parse_head(&buffer)? {
                break head;
            }
            if !read_more(&mut stream, &mut buffer).await? {
                return Err(HttpError::new(HttpErrorKind::Protocol, "the connection closed before a response"));
            }
        };
        buffer.drain(..head_len);
        // An interim answer (100 Continue, 103 Early Hints) precedes the real one.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let body = read_body(&mut stream, &mut buffer, status, &headers).await?;
        return Ok(Response { status, headers, body });
    }
}

type Head = (usize, u16, Vec<(String, String)>);

fn parse_head(buffer: &[u8]) -> Result<Option<Head>, HttpError> {
    let mut slots = [httparse::EMPTY_HEADER; 96];
    let mut response = httparse::Response::new(&mut slots);
    match response.parse(buffer) {
        Ok(httparse::Status::Complete(len)) => {
            let status = response.code.unwrap_or_default();
            let headers = response
                .headers
                .iter()
                .map(|h| (h.name.to_owned(), String::from_utf8_lossy(h.value).trim().to_owned()))
                .collect();
            Ok(Some((len, status, headers)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(error) => Err(HttpError::new(HttpErrorKind::Protocol, error.to_string())),
    }
}

async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    status: u16,
    headers: &[(String, String)],
) -> Result<Vec<u8>, HttpError> {
    if status == 204 || status == 304 {
        return Ok(Vec::new());
    }
    let header = |name: &str| headers.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|(_, v)| v);
    if header("Transfer-Encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked")) {
        loop {
            if let Some(body) = decode_chunked(buffer)? {
                return Ok(body);
            }
            if !read_more(stream, buffer).await? {
                return Err(HttpError::new(HttpErrorKind::Protocol, "the connection closed inside a chunked body"));
            }
        }
    }
    if let Some(length) = header("Content-Length") {
        let length: usize =
            length.parse().map_err(|_| HttpError::new(HttpErrorKind::Protocol, "an unreadable Content-Length"))?;
        while buffer.len() < length {
            if !read_more(stream, buffer).await? {
                return Err(HttpError::new(HttpErrorKind::Protocol, "the connection closed inside the body"));
            }
        }
        buffer.truncate(length);
        return Ok(std::mem::take(buffer));
    }
    while read_more(stream, buffer).await? {}
    Ok(std::mem::take(buffer))
}

/// The body once its last chunk has arrived, or None while it is still coming. Trailers are not read: the
/// connection closes after this response.
fn decode_chunked(data: &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
    let mut body = Vec::new();
    let mut at = 0;
    loop {
        let (start, size) = match httparse::parse_chunk_size(&data[at..]) {
            Ok(httparse::Status::Complete(sized)) => sized,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(_) => return Err(HttpError::new(HttpErrorKind::Protocol, "an unreadable chunk size")),
        };
        if size == 0 {
            return Ok(Some(body));
        }
        let size = usize::try_from(size).map_err(|_| HttpError::new(HttpErrorKind::Protocol, "a chunk too large"))?;
        let chunk = at + start;
        let Some(end) = chunk.checked_add(size).filter(|end| end + 2 <= data.len()) else {
            return Ok(None);
        };
        body.extend_from_slice(&data[chunk..end]);
        at = end + 2;
    }
}

/// Read what has arrived into `buffer`; false at the end of the stream.
async fn read_more<S: AsyncRead + Unpin>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<bool, HttpError> {
    let mut chunk = [0u8; 16 * 1024];
    match stream.read(&mut chunk).await {
        Ok(0) => Ok(false),
        Ok(read) => {
            buffer.extend_from_slice(&chunk[..read]);
            Ok(true)
        }
        // Servers often close a TLS connection without close_notify once a close-delimited response is sent; the
        // lengths above still catch a body cut short.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn io_error(error: std::io::Error) -> HttpError {
    HttpError::new(HttpErrorKind::Io, error.to_string())
}

/// A local HTTP server answering canned responses, for the adapters' tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    pub(crate) struct Recorded {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Recorded {
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    /// What the server answers: a status, extra headers, and a body.
    pub(crate) type Canned = (u16, Vec<(String, String)>, Vec<u8>);

    pub(crate) fn json(status: u16, body: serde_json::Value) -> Canned {
        (status, vec![], body.to_string().into_bytes())
    }

    pub(crate) struct FakeServer {
        pub base: String,
        pub requests: Arc<Mutex<Vec<Recorded>>>,
    }

    impl FakeServer {
        pub(crate) async fn start<F>(handler: F) -> Self
        where
            F: Fn(&Recorded, &str) -> Canned + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (recorded, handler, own) = (requests.clone(), Arc::new(handler), base.clone());
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let (recorded, handler, own) = (recorded.clone(), handler.clone(), own.clone());
                    tokio::spawn(async move {
                        let mut buffer = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let request = loop {
                            let read = socket.read(&mut chunk).await.unwrap_or(0);
                            if read == 0 {
                                return;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                            let mut slots = [httparse::EMPTY_HEADER; 64];
                            let mut parsed = httparse::Request::new(&mut slots);
                            if let Ok(httparse::Status::Complete(len)) = parsed.parse(&buffer) {
                                let headers: Vec<(String, String)> = parsed
                                    .headers
                                    .iter()
                                    .map(|h| (h.name.to_owned(), String::from_utf8_lossy(h.value).into_owned()))
                                    .collect();
                                let length = headers
                                    .iter()
                                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .map_or(0, |(_, value)| value.parse().unwrap());
                                if buffer.len() >= len + length {
                                    break Recorded {
                                        method: parsed.method.unwrap().to_owned(),
                                        path: parsed.path.unwrap().to_owned(),
                                        headers,
                                        body: buffer[len..len + length].to_vec(),
                                    };
                                }
                            }
                        };
                        recorded.lock().unwrap().push(request.clone());
                        // A handler may block to hold its answer back, as a slow service does.
                        let (status, headers, body) =
                            tokio::task::spawn_blocking(move || handler(&request, &own)).await.unwrap();
                        let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                        for (name, value) in headers {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            Self { base, requests }
        }

        pub(crate) fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::fake::{FakeServer, json};
    use super::*;

    async fn raw_server(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            socket.write_all(reply).await.unwrap();
        });
        base
    }

    #[tokio::test]
    async fn sends_json_and_reads_a_sized_body() {
        let server = FakeServer::start(|_, _| json(201, serde_json::json!({"ok": true}))).await;
        let url = format!("{}/api?x=1", server.base);
        let response =
            request("POST", &url, &[("X-Key", "k")], Some(r#"{"b":1,"a":2}"#), Duration::from_secs(5)).await.unwrap();
        assert_eq!((response.status, response.json().unwrap()), (201, serde_json::json!({"ok": true})));
        let sent = &server.requests()[0];
        assert_eq!((sent.method.as_str(), sent.path.as_str()), ("POST", "/api?x=1"));
        assert_eq!(sent.body, br#"{"b":1,"a":2}"#);
        assert_eq!(sent.json(), serde_json::json!({"a": 2, "b": 1}));
        assert!(sent.headers.iter().any(|(name, value)| name == "X-Key" && value == "k"));
    }

    #[tokio::test]
    async fn reads_chunked_and_close_delimited_bodies_past_an_interim_answer() {
        let chunked = raw_server(
            concat!(
                "HTTP/1.1 100 Continue\r\n\r\n",
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                "4\r\nwiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n",
            )
            .as_bytes(),
        )
        .await;
        let response = request("GET", &chunked, &[], None, Duration::from_secs(5)).await.unwrap();
        assert_eq!((response.status, response.body.as_slice()), (200, &b"wikipedia"[..]));
        let closed = raw_server(b"HTTP/1.1 503 Busy\r\nRetry-After: 3\r\n\r\nlater").await;
        let response = request("GET", &closed, &[], None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(
            (response.status, response.header("retry-after"), response.body.as_slice()),
            (503, Some("3"), &b"later"[..])
        );
    }

    #[tokio::test]
    async fn failures_name_their_kind_and_never_the_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = format!("http://user:secret@{}/x", listener.local_addr().unwrap());
        drop(listener);
        let error = request("GET", &closed, &[("Key", "secret")], None, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(error.kind, HttpErrorKind::Connect);
        assert!(error.is_transient() && !error.to_string().contains("secret"));
        let cut = raw_server(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").await;
        assert_eq!(
            request("GET", &cut, &[], None, Duration::from_secs(5)).await.unwrap_err().kind,
            HttpErrorKind::Protocol
        );
        let invalid = request("GET", "ftp://x/", &[], None, Duration::from_secs(5)).await.unwrap_err();
        assert!(!invalid.is_transient());
        let header = request("GET", "http://x/", &[("K", "a\r\nb")], None, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(header.kind, HttpErrorKind::InvalidRequest);
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let error = request("GET", &url, &[], None, Duration::from_millis(100)).await.unwrap_err();
        assert_eq!(error.kind, HttpErrorKind::Timeout);
        drop(listener);
    }

    #[test]
    fn urls_split_into_host_port_and_path() {
        let target = parse_url("https://[::1]:8443?q=1#frag").unwrap();
        assert_eq!((target.tls, target.host.as_str(), target.port, target.path.as_str()), (true, "::1", 8443, "/?q=1"));
        let target = parse_url("http://api.test/v3/browsers").unwrap();
        assert_eq!((target.port, target.authority.as_str(), target.path.as_str()), (80, "api.test", "/v3/browsers"));
    }
}
