//! A real `BrowserSession` driven through the fastbrowse extension: headless Chrome for Testing loads the unpacked
//! extension, Chrome starts this crate's `fastbrowse native-host`, and the run reaches the browser only through the
//! host's socket. Skips when Chrome for Testing is not installed (branded Chrome ignores `--load-extension`).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fastbrowse_browser::extension::SCHEME;
use fastbrowse_browser::extension::protocol::EXTENSION_IDS;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_core::artifacts::DirectorySink;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Attachment, BrowserConnection, Bytes, Operation};
use fastbrowse_core::page::{Action, Observation};
use serde_json::json;

const FORM: &str = r#"<!doctype html><title>Form</title>
<label>Name <input id="name"></label>
<button onclick="document.title = 'Sent ' + document.getElementById('name').value">Send</button>
<input id="attachment" type="file" hidden>
<button onclick="document.getElementById('attachment').click()">Attach file</button>"#;

/// Chrome for Testing: `FASTBROWSE_TEST_EXTENSION_CHROME`, else the newest Playwright download.
fn chrome_for_testing() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FASTBROWSE_TEST_EXTENSION_CHROME") {
        return Some(PathBuf::from(path));
    }
    let root = PathBuf::from(std::env::var_os("HOME")?).join(".cache/ms-playwright");
    let mut found: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("chromium-"))
        .map(|e| e.path().join("chrome-linux64/chrome"))
        .filter(|p| p.exists())
        .collect();
    found.sort();
    found.pop()
}

/// Serves the form page on every path.
fn serve_form() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{FORM}",
                FORM.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    port
}

/// Chrome with the extension loaded and the host registered in its throwaway profile; killed on drop.
struct Browser {
    child: Child,
    hosts: PathBuf,
    root: PathBuf,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.hosts);
    }
}

impl Browser {
    fn launch(chrome: &Path, root: &Path, extension: &Path) -> Browser {
        let profile = root.join("profile");
        // A socket path has a 108-byte limit, which a target directory can pass: the host listens somewhere short.
        let hosts = std::env::temp_dir().join(format!("fbx-{}", std::process::id()));
        let manifests = profile.join("NativeMessagingHosts");
        std::fs::create_dir_all(&manifests).expect("profile");
        let launcher = root.join("host.sh");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nFASTBROWSE_EXTENSION_DIR='{}' exec '{}' native-host \"$@\" 2>>'{}'\n",
                hosts.display(),
                env!("CARGO_BIN_EXE_fastbrowse"),
                root.join("host.err").display()
            ),
        )
        .expect("launcher");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let manifest = json!({
            "name": "dev.fastbrowse.host",
            "description": "fastbrowse test host",
            "path": launcher,
            "type": "stdio",
            "allowed_origins": [format!("chrome-extension://{}/", EXTENSION_IDS[0])],
        });
        std::fs::write(manifests.join("dev.fastbrowse.host.json"), manifest.to_string()).expect("manifest");
        let child = Command::new(chrome)
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-features=DisableLoadExtensionCommandLineSwitch",
                // Only for the test to act as the user would, closing a tab from outside the run.
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--load-extension={}", extension.canonicalize().expect("extension dir").display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Chrome starts");
        Browser { child, hosts, root: root.to_owned() }
    }

    /// The pages open in the browser, as `(id, url)`, read over its DevTools port.
    fn pages(&self) -> Vec<(String, String)> {
        let listed: serde_json::Value = serde_json::from_str(&self.devtools("/json/list")).expect("target list");
        listed
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page")
            .map(|t| {
                (t["id"].as_str().unwrap_or_default().to_owned(), t["url"].as_str().unwrap_or_default().to_owned())
            })
            .collect()
    }

    /// One plain HTTP GET to the browser's DevTools port. Chrome keeps the connection open, so the body is read by
    /// its length.
    fn devtools(&self, path: &str) -> String {
        let active = std::fs::read_to_string(self.root.join("profile/DevToolsActivePort")).expect("DevToolsActivePort");
        let port = active.lines().next().expect("port line");
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("DevTools port");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").expect("request");
        let mut reader = BufReader::new(stream);
        let mut length = 0;
        let mut line = String::new();
        while reader.read_line(&mut line).expect("header") > 2 {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("length");
            }
            line.clear();
        }
        let mut body = vec![0; length];
        std::io::Read::read_exact(&mut reader, &mut body).expect("body");
        String::from_utf8(body).expect("utf-8")
    }

    /// The host's record, once the extension has connected and the host listens.
    async fn record(&self) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = std::fs::read_dir(&self.hosts).ok().and_then(|entries| {
                entries.flatten().map(|e| e.path()).find(|p| {
                    p.extension().is_some_and(|x| x == "json")
                        && !p.file_name().unwrap().to_string_lossy().starts_with('.')
                })
            });
            if let Some(record) = found {
                return record;
            }
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(self.root.join("host.err")).unwrap_or_default();
                panic!("the extension never reached the native host; host stderr: {log}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn open(record: &Path, task: &str, artifacts: &Path) -> (BrowserSession, CdpPage) {
    open_with(record, task, artifacts, SessionOptions::default()).await
}

async fn open_with(record: &Path, task: &str, artifacts: &Path, options: SessionOptions) -> (BrowserSession, CdpPage) {
    let connection = BrowserConnection::new(format!("{SCHEME}{}#{task}", record.display()), false);
    let sink = Arc::new(DirectorySink::new(artifacts).expect("sink"));
    let session = BrowserSession::new(connection, sink, options);
    session.open().await.expect("session opens through the extension");
    let page = CdpPage::new(session.clone(), Config::default());
    (session, page)
}

fn control<'a>(observation: &'a Observation, label: &str) -> &'a fastbrowse_core::page::Control {
    observation.controls.iter().find(|c| c.label.contains(label)).unwrap_or_else(|| {
        panic!("no {label:?} in {:?}", observation.controls.iter().map(|c| &c.label).collect::<Vec<_>>())
    })
}

async fn fill_and_send(page: &CdpPage, session: &BrowserSession, url: &str, name: &str) {
    page.navigate(url, None).await.expect("navigate");
    let observation = page.observe().await.expect("observe");
    let fill = Action {
        target_id: Some(control(&observation, "Name").id.clone()),
        text: Some(name.to_owned()),
        ..Action::new(Operation::Fill)
    };
    page.act(&fill, &observation).await.expect("fill");
    let observation = page.observe().await.expect("observe");
    let click = Action { target_id: Some(control(&observation, "Send").id.clone()), ..Action::new(Operation::Click) };
    page.act(&click, &observation).await.expect("click");
    let session_id = session.active_session_id().expect("active session");
    let title = page.evaluate(&session_id, "document.title").await.expect("evaluate");
    assert_eq!(title, json!(format!("Sent {name}")));
}

#[tokio::test(flavor = "current_thread")]
async fn the_extension_reports_its_manifest_version_and_supported_commands() {
    let Some(chrome) = chrome_for_testing() else {
        eprintln!("Chrome for Testing is not installed; skipping the extension handshake test");
        return;
    };
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("extension-hello-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fastbrowse-browser/extension");
    let browser = Browser::launch(&chrome, &root, &extension);
    let record: fastbrowse_browser::extension::protocol::HostRecord =
        serde_json::from_slice(&std::fs::read(browser.record().await).expect("record")).expect("host record");
    let endpoint = fastbrowse_browser::extension::shim::connect(&record, "handshake", Default::default())
        .await
        .expect("compatible extension");
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../fastbrowse-browser/extension/manifest.json")).expect("manifest");
    assert_eq!(endpoint.hello["extensionVersion"], manifest["version"]);
    assert_eq!(endpoint.hello["protocol"], fastbrowse_browser::extension::protocol::PROTOCOL);
    assert_eq!(endpoint.hello["methods"], json!(fastbrowse_browser::extension::protocol::allowed_methods()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_run_drives_a_tab_through_the_extension() {
    let Some(chrome) = chrome_for_testing() else {
        eprintln!("Chrome for Testing is not installed; skipping the extension backend test");
        return;
    };
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("extension-backend-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fastbrowse-browser/extension");
    let browser = Browser::launch(&chrome, &root, &extension);
    let record = browser.record().await;
    let url = format!("http://127.0.0.1:{}/form", serve_form());

    // One run, then two at once, each in its own tabs.
    let (session, page) = open(&record, "first run", &root.join("a1")).await;
    fill_and_send(&page, &session, &url, "Ada").await;
    let observation = page.observe().await.expect("observe");
    let opener = control(&observation, "Attach file");
    let click = Action { target_id: Some(opener.id.clone()), ..Action::new(Operation::Click) };
    page.act(&click, &observation).await.expect("open chooser");
    let observation = page.observe().await.expect("observe intercepted chooser");
    let opener = control(&observation, "Attach file");
    assert!(opener.operations.contains(&Operation::Upload));
    let upload = Action {
        target_id: Some(opener.id.clone()),
        files: vec![Attachment {
            name: "receipt.txt".into(),
            mime_type: "text/plain".into(),
            content: Bytes(b"receipt".to_vec()),
        }],
        ..Action::new(Operation::Upload)
    };
    page.act(&upload, &observation).await.expect("attach");
    let active = session.active_session_id().expect("active session");
    let filename = page.evaluate(&active, "document.querySelector('#attachment').files[0]?.name").await.expect("file");
    assert_eq!(filename, json!("receipt.txt"));
    session.close().await.expect("close");

    let (b1, b2) = (root.join("b1"), root.join("b2"));
    let (one, two) = tokio::join!(open(&record, "run one", &b1), open(&record, "run two", &b2));
    tokio::join!(fill_and_send(&one.1, &one.0, &url, "Grace"), fill_and_send(&two.1, &two.0, &url, "Edsger"));
    one.0.close().await.expect("close");
    two.0.close().await.expect("close");
    // A finished run closes the tabs it opened.
    let left = browser.pages();
    assert!(left.iter().all(|(_, page)| !page.starts_with(&url)), "tabs left open: {left:?}");

    // Closing the run's last tab, as the user would, stops the run.
    let stopped = Arc::new(std::sync::Mutex::new(None::<String>));
    let told = stopped.clone();
    let options = SessionOptions {
        on_stop: Some(Arc::new(move |reason: &str| *told.lock().unwrap() = Some(reason.to_owned()))),
        ..SessionOptions::default()
    };
    let (session, page) = open_with(&record, "stopped run", &root.join("c1"), options).await;
    page.navigate(&format!("{url}?stop"), None).await.expect("navigate");
    let (tab, _) = browser.pages().into_iter().find(|(_, page)| page.ends_with("?stop")).expect("the run's tab");
    browser.devtools(&format!("/json/close/{tab}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while stopped.lock().unwrap().is_none() {
        assert!(Instant::now() < deadline, "closing the run's tab did not stop it");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(stopped.lock().unwrap().as_deref(), Some("the run was stopped because its last tab was closed"));
    assert!(page.observe().await.is_err(), "a stopped run's browser still answers");
    let _ = session.close().await;
    drop(browser);

    // An installed extension whose hello lists the interception command but whose relay rejects it, so the
    // connection is accepted and only the command fails: the run must stop rather than leave a native picker.
    // Dropping the method from `allowed-methods.json` would drop it from hello too, and the shim would refuse the
    // connection before the command was ever sent.
    let stale_extension = root.join("stale-extension");
    std::fs::create_dir(&stale_extension).expect("extension folder");
    for entry in std::fs::read_dir(extension).expect("extension files").flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), stale_extension.join(entry.file_name())).expect("copy extension file");
        }
    }
    let background = stale_extension.join("background.js");
    let script = std::fs::read_to_string(&background).expect("background script");
    let relay = "if (!(await allowedMethods()).has(method)) throw";
    assert!(script.contains(relay), "the relay's method check moved");
    let refusing = script.replacen(
        relay,
        "if (method === 'Page.setInterceptFileChooserDialog' || !(await allowedMethods()).has(method)) throw",
        1,
    );
    std::fs::write(background, refusing).expect("refusing relay");
    let stale_root = root.join("stale");
    let browser = Browser::launch(&chrome, &stale_root, &stale_extension);
    let record = browser.record().await;
    let connection = BrowserConnection::new(format!("{SCHEME}{}#file run", record.display()), false);
    let sink = Arc::new(DirectorySink::new(root.join("files")).expect("sink"));
    let options = SessionOptions { has_attachments: true, ..SessionOptions::default() };
    let session = BrowserSession::new(connection, sink, options);
    let error = session.open().await.expect_err("a file run must stop without interception");
    assert!(error.to_string().contains("fastbrowse extension path"), "{error}");
    drop(browser);
    let _ = std::fs::remove_dir_all(&root);
}
