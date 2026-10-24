//! What tests/browser/conftest.py provides, for the Rust suites: a real headless Chrome shared by a test binary, the
//! fixture sites served on two hosts (the main site on 127.0.0.1, its iframe on localhost, so Chrome puts the frame
//! in a process of its own), an in-memory artifact sink, and a session and page per test.
//!
//! Without Chrome every browser test returns early, as Python's skip does, after saying so on stderr.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use data_encoding::HEXLOWER;
use fastbrowse_browser::adapters::local_chrome::{LocalChromeProcess, find_chrome};
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::shared::{
    Artifact, ArtifactKind, ArtifactSink, BoxError, BoxFuture, BrowserConnection, Config, LocalChrome, Observation,
};

pub fn sites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/browser/sites")
}

/// Test double for `ArtifactSink`: keeps every artifact in memory for assertions.
#[derive(Default)]
pub struct RecordingArtifactSink {
    pub artifacts: Mutex<Vec<Artifact>>,
    pub contents: Mutex<Vec<Vec<u8>>>,
}

impl ArtifactSink for RecordingArtifactSink {
    fn put<'a>(
        &'a self,
        kind: ArtifactKind,
        name: &'a str,
        mime_type: &'a str,
        content: Vec<u8>,
    ) -> BoxFuture<'a, Result<Artifact, BoxError>> {
        Box::pin(async move {
            let digest = ring::digest::digest(&ring::digest::SHA256, &content);
            let artifact = Artifact {
                kind,
                name: name.to_owned(),
                mime_type: mime_type.to_owned(),
                size_bytes: content.len() as u64,
                sha256: HEXLOWER.encode(digest.as_ref()),
                uri: format!("memory://{name}"),
            };
            self.artifacts.lock().unwrap().push(artifact.clone());
            self.contents.lock().unwrap().push(content);
            Ok(artifact)
        })
    }
}

pub struct Sites {
    pub main: String,
    pub iframe: String,
    /// Bodies the fixture pages POSTed, oldest first.
    pub posted: Arc<Mutex<Vec<String>>>,
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn serve_one(mut stream: TcpStream, directory: &Path, iframe_origin: Option<&str>, posted: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/").to_owned());
    if method == "POST" {
        // What a page sent is the one view of it a test has when the browser belongs to another process.
        let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        posted.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
        respond(&mut stream, 204, &[], b"");
        return;
    }
    let html = ("Content-Type", "text/html".to_owned());
    if let Some(status) = path.strip_prefix("/http-error/") {
        let body = b"<html><body>Sorry</body></html>";
        respond(&mut stream, status.parse().unwrap_or(500), &[html, ("Content-Length", body.len().to_string())], body);
        return;
    }
    if path.starts_with("/report.csv") || path == "/export-report" {
        let mut body = b"name,total\nReport,139.79\n".to_vec();
        if path != "/report.csv?short" && path != "/report.csv?partial" {
            body.extend(b"item,1\n".repeat(10000));
        }
        let mut headers = vec![];
        if path == "/report.csv?partial" {
            headers.push(("Content-Range", format!("bytes 0-{}/1000", body.len() - 1)));
        }
        let mime = if path == "/export-report" { "text/plain" } else { "text/csv; charset=utf-8" };
        headers.push(("Content-Type", mime.to_owned()));
        if path == "/export-report" {
            headers.push(("Content-Disposition", "attachment; filename=\"report.CSV\"".to_owned()));
        }
        headers.push(("Content-Length", body.len().to_string()));
        respond(&mut stream, if path == "/report.csv?partial" { 206 } else { 200 }, &headers, &body);
        return;
    }
    if path.starts_with("/files/") {
        let body = b"PK fixture release archive";
        let headers = [("Content-Type", "application/zip".to_owned()), ("Content-Length", body.len().to_string())];
        respond(&mut stream, 200, &headers, body);
        return;
    }
    if path == "/download" {
        let body = b"fastbrowse fixture attachment bytes for download checksum test";
        let headers = [
            ("Content-Type", "application/octet-stream".to_owned()),
            ("Content-Disposition", "attachment; filename=\"spike.bin\"".to_owned()),
            ("Content-Length", body.len().to_string()),
        ];
        respond(&mut stream, 200, &headers, body);
        return;
    }
    let file = path.split('?').next().unwrap_or("/");
    if (file == "/" || file == "/index.html")
        && let Some(origin) = iframe_origin
    {
        let content = std::fs::read_to_string(directory.join("index.html")).unwrap_or_default();
        let body = content.replace("__IFRAME_ORIGIN__", origin).into_bytes();
        respond(&mut stream, 200, &[html, ("Content-Length", body.len().to_string())], &body);
        return;
    }
    let target = directory.join(file.trim_start_matches('/'));
    match std::fs::read(&target) {
        Ok(body) if target.is_file() => {
            respond(&mut stream, 200, &[html, ("Content-Length", body.len().to_string())], &body);
        }
        _ => respond(&mut stream, 404, &[], b""),
    }
}

/// Serve `directory` as conftest's `_handler_for` does, on 127.0.0.1, returning the port.
pub fn serve(directory: PathBuf, iframe_origin: Option<String>, posted: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (directory, origin, posted) = (directory.clone(), iframe_origin.clone(), posted.clone());
            thread::spawn(move || serve_one(stream, &directory, origin.as_deref(), &posted));
        }
    });
    port
}

pub fn sites() -> &'static Sites {
    static SITES: OnceLock<Sites> = OnceLock::new();
    SITES.get_or_init(|| {
        let posted = Arc::new(Mutex::new(vec![]));
        let iframe = format!("http://localhost:{}", serve(sites_dir().join("iframe"), None, posted.clone()));
        let main =
            format!("http://127.0.0.1:{}", serve(sites_dir().join("main"), Some(iframe.clone()), posted.clone()));
        Sites { main, iframe, posted }
    })
}

/// The shared headless Chrome's connection, or None (with a note) when Chrome is not installed.
pub fn chrome() -> Option<&'static BrowserConnection> {
    static CHROME: OnceLock<Option<BrowserConnection>> = OnceLock::new();
    CHROME
        .get_or_init(|| {
            if find_chrome(None).is_none() {
                eprintln!("Chrome is not installed; skipping browser tests");
                return None;
            }
            // Launched on a thread and runtime of its own, since each test's runtime ends with the test.
            let launched = thread::spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
                let (process, connection) =
                    runtime.block_on(LocalChromeProcess::launch(&LocalChrome::default())).expect("Chrome launches");
                watch_for_exit(process.pid());
                // The handle is kept for the binary's life: dropping it would end Chrome under the next test.
                std::mem::forget(process);
                connection
            });
            Some(launched.join().expect("launch thread"))
        })
        .as_ref()
}

/// A test binary has no teardown for statics, so a watcher ends Chrome when the binary exits.
fn watch_for_exit(chrome: Option<u32>) {
    let Some(chrome) = chrome else { return };
    let me = std::process::id();
    let script = format!("while kill -0 {me} 2>/dev/null; do sleep 0.5; done; kill {chrome} 2>/dev/null");
    let _ = Command::new("sh").args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).spawn();
}

pub struct Harness {
    pub session: BrowserSession,
    pub page: CdpPage,
    pub sink: Arc<RecordingArtifactSink>,
    _turn: Turn,
}

/// One browser test at a time per binary, as pytest runs them: a tab behind another is throttled, so parallel tests
/// measured settles and scroll waits that no run sees, and Chrome refuses a burst of DevTools handshakes. A test that
/// opens a second session already holds the turn, so the turn is counted per test thread rather than taken twice.
pub struct Turn(Option<tokio::sync::OwnedSemaphorePermit>);

thread_local! {
    static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Wait for this test's turn on the shared Chrome; held until the returned value drops.
pub async fn turn() -> Turn {
    static TURNS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let held = HELD.with(|held| held.replace(held.get() + 1));
    if held > 0 {
        return Turn(None);
    }
    let turns = TURNS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone();
    Turn(Some(turns.acquire_owned().await.expect("turns are never closed")))
}

impl Drop for Turn {
    fn drop(&mut self) {
        HELD.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

impl Harness {
    pub async fn close(self) {
        let _ = self.session.close().await;
    }

    pub async fn goto(&self, path: &str) {
        self.page.navigate(&format!("{}{path}", sites().main), None).await.expect("navigate");
    }

    pub async fn observe(&self) -> Observation {
        self.page.observe().await.expect("observe")
    }
}

/// A session and page on the shared Chrome, with `config` and `options`; None without Chrome.
pub async fn harness_with(config: Config, options: SessionOptions) -> Option<Harness> {
    harness_on(chrome()?.clone(), config, options).await
}

pub async fn harness_on(connection: BrowserConnection, config: Config, options: SessionOptions) -> Option<Harness> {
    let turn = turn().await;
    let sink = Arc::new(RecordingArtifactSink::default());
    let session = BrowserSession::new(connection, sink.clone(), options);
    session.open().await.expect("session opens");
    let page = CdpPage::new(session.clone(), config);
    Some(Harness { session, page, sink, _turn: turn })
}

/// conftest's `page` fixture: a fresh session with the default config.
pub async fn harness() -> Option<Harness> {
    harness_with(Config::default(), SessionOptions::default()).await
}

/// The control whose label is `label`, as tests look one up.
pub fn control<'a>(observation: &'a Observation, label: &str) -> &'a fastbrowse_browser::shared::Control {
    observation.controls.iter().find(|c| c.label == label).unwrap_or_else(|| {
        panic!(
            "no control labelled {label:?} in {:?}",
            observation.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
        )
    })
}
