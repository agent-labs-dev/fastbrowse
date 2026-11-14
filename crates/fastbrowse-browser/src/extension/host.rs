//! `fastbrowse native-host`: Chrome starts it over stdio when the extension connects, and it lets runs reach the
//! extension through a per-user local socket.
//!
//! The host reads nothing it relays. It tags each run's lines with the run's connection id, writes them to Chrome in
//! native messaging's framing, and routes the extension's replies and events back by that id. When a run's
//! connection closes, it tells the extension to release the run's tabs. When Chrome closes stdin, it exits and
//! removes its socket and record.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::protocol::{EXTENSION_IDS, HostRecord, MAX_FROM_EXTENSION, PROTOCOL, frames_for_extension, host_dir};

/// Why the host would not start.
#[derive(Debug)]
pub struct HostError(pub String);

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether Chrome started this host for one of our extensions. Chrome passes the caller's origin as the first
/// argument, after checking it against the manifest's `allowed_origins`; this refuses anything else as well.
pub fn caller_allowed(origin: &str) -> bool {
    EXTENSION_IDS.iter().any(|id| origin.trim_end_matches('/') == format!("chrome-extension://{id}"))
}

/// One native-messaging frame from Chrome, or `None` at the end of input.
pub async fn read_frame(input: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Value>> {
    let mut head = [0u8; 4];
    match input.read_exact(&mut head).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_ne_bytes(head) as usize;
    if length > MAX_FROM_EXTENSION {
        return Err(std::io::Error::other("a frame from the extension exceeds 64 MiB"));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// Write one message to Chrome, split into frames it accepts.
pub async fn write_frames(output: &mut (impl AsyncWrite + Unpin), message: &str, chunk_id: u64) -> std::io::Result<()> {
    for frame in frames_for_extension(message, chunk_id) {
        output.write_all(&u32::try_from(frame.len()).unwrap_or(u32::MAX).to_ne_bytes()).await?;
        output.write_all(frame.as_bytes()).await?;
    }
    output.flush().await
}

/// Files the host leaves for runs to find it, removed when it exits.
struct Listing {
    socket: PathBuf,
    record: PathBuf,
}

impl Drop for Listing {
    fn drop(&mut self) {
        // A host the extension started while this one was ending (it reconnects when its label changes) has taken
        // over the record and the socket path; they are only this host's to remove while the record names it.
        if !listed_by(&self.record, std::process::id()) {
            return;
        }
        let _ = std::fs::remove_file(&self.record);
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn listed_by(record: &Path, pid: u32) -> bool {
    std::fs::read(record)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HostRecord>(&bytes).ok())
        .is_some_and(|listed| listed.pid == pid)
}

fn private_dir(dir: &Path) -> Result<(), HostError> {
    std::fs::create_dir_all(dir).map_err(|e| HostError(format!("cannot create {}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| HostError(format!("cannot make {} private: {e}", dir.display())))?;
    }
    Ok(())
}

/// A browser id is the extension's own random id; anything else would let a message name a path.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Run the host on this process's stdin and stdout until Chrome closes them.
pub async fn run(origin: &str) -> Result<(), HostError> {
    if !caller_allowed(origin) {
        return Err(HostError(format!("{origin} is not a fastbrowse extension")));
    }
    let mut input = tokio::io::stdin();
    let output = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    // The extension introduces itself first: its browser id names the socket.
    let hello = read_frame(&mut input)
        .await
        .map_err(|e| HostError(format!("reading the extension's hello failed: {e}")))?
        .ok_or_else(|| HostError("the extension closed the port before saying hello".into()))?;
    let browser_id = hello["browserId"].as_str().unwrap_or_default().to_owned();
    if !safe_id(&browser_id) {
        return Err(HostError("the extension's hello names no usable browser id".into()));
    }
    let dir = host_dir();
    private_dir(&dir)?;
    let record = HostRecord {
        browser_id: browser_id.clone(),
        label: hello["label"].as_str().unwrap_or("Chrome").to_owned(),
        browser: hello["browser"].as_str().unwrap_or("Chrome").to_owned(),
        version: hello["version"].as_str().unwrap_or_default().to_owned(),
        protocol: hello["protocol"].as_u64().and_then(|p| u32::try_from(p).ok()).unwrap_or(PROTOCOL),
        pid: std::process::id(),
        browser_pid: parent_browser_pid(),
        socket: socket_path(&dir, &browser_id),
    };
    let listener = listen(&record.socket)?;
    let listing = Listing { socket: record.socket.clone(), record: dir.join(format!("{browser_id}.json")) };
    // Written only once the socket listens, through a rename, so a run that finds the record can dial at once.
    let staged = dir.join(format!(".{browser_id}.json.{}", std::process::id()));
    std::fs::write(&staged, serde_json::to_vec(&record).unwrap_or_default())
        .and_then(|()| std::fs::rename(&staged, &listing.record))
        .map_err(|e| HostError(format!("cannot write {}: {e}", listing.record.display())))?;

    let runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>> = Arc::default();
    let chunks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (to_chrome, mut chrome_queue) = mpsc::unbounded_channel::<String>();
    let writer = Arc::clone(&output);
    let chunk_ids = Arc::clone(&chunks);
    tokio::spawn(async move {
        while let Some(message) = chrome_queue.recv().await {
            let id = chunk_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if write_frames(&mut *writer.lock().await, &message, id).await.is_err() {
                return;
            }
        }
    });
    let accepting = accept_runs(listener, record.socket.clone(), Arc::clone(&runs), to_chrome);
    let from_chrome = async {
        while let Ok(Some(message)) = read_frame(&mut input).await {
            let Some(conn) = message["conn"].as_u64() else { continue };
            let run = runs.lock().unwrap().get(&conn).cloned();
            if let Some(run) = run {
                let _ = run.send(message.to_string());
            }
        }
    };
    tokio::select! {
        () = accepting => {}
        () = from_chrome => {}
    }
    drop(listing);
    Ok(())
}

fn socket_path(dir: &Path, browser_id: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\fastbrowse-{browser_id}"))
    } else {
        dir.join(format!("{browser_id}.sock"))
    }
}

#[cfg(unix)]
fn listen(socket: &Path) -> Result<tokio::net::UnixListener, HostError> {
    // A host that was killed leaves its socket file, which would refuse every run; this host replaces it.
    if socket.exists() {
        let _ = std::fs::remove_file(socket);
    }
    let listener = tokio::net::UnixListener::bind(socket).map_err(|e| {
        HostError(format!(
            "cannot listen on {}: {e}. A socket path must be under about 100 bytes; set FASTBROWSE_EXTENSION_DIR to a \
             shorter directory",
            socket.display()
        ))
    })?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| HostError(format!("cannot make {} private: {e}", socket.display())))?;
    Ok(listener)
}

#[cfg(unix)]
async fn accept_runs(
    listener: tokio::net::UnixListener,
    _socket: PathBuf,
    runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>>,
    to_chrome: mpsc::UnboundedSender<String>,
) {
    let mut next = 0u64;
    while let Ok((stream, _)) = listener.accept().await {
        next += 1;
        tokio::spawn(serve_run(stream, next, Arc::clone(&runs), to_chrome.clone()));
    }
}

#[cfg(windows)]
fn listen(socket: &Path) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, HostError> {
    tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(socket)
        .map_err(|e| HostError(format!("cannot listen on {}: {e}", socket.display())))
}

#[cfg(windows)]
async fn accept_runs(
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
    pipe: PathBuf,
    runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>>,
    to_chrome: mpsc::UnboundedSender<String>,
) {
    let mut next = 0u64;
    loop {
        if server.connect().await.is_err() {
            return;
        }
        // Each client takes the instance it connected to; the next waits on a fresh one.
        let Ok(fresh) = tokio::net::windows::named_pipe::ServerOptions::new().create(&pipe) else { return };
        let connected = std::mem::replace(&mut server, fresh);
        next += 1;
        tokio::spawn(serve_run(connected, next, Arc::clone(&runs), to_chrome.clone()));
    }
}

/// One run's connection: its lines go to Chrome tagged with `conn`; Chrome's lines for it come back.
async fn serve_run<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    conn: u64,
    runs: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>>,
    to_chrome: mpsc::UnboundedSender<String>,
) {
    let (read, mut write) = tokio::io::split(stream);
    let (to_run, mut run_queue) = mpsc::unbounded_channel::<String>();
    runs.lock().unwrap().insert(conn, to_run);
    let writing = tokio::spawn(async move {
        while let Some(line) = run_queue.recv().await {
            if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                return;
            }
        }
    });
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else { continue };
        message["conn"] = conn.into();
        if to_chrome.send(message.to_string()).is_err() {
            break;
        }
    }
    runs.lock().unwrap().remove(&conn);
    // The run is gone: the extension detaches and closes the tabs it opened.
    let _ = to_chrome.send(json!({"op": "release", "conn": conn}).to_string());
    writing.abort();
}

// Chrome launches the exec-based native-host launcher, so no page-supplied identity enters the cursor mapping.
fn parent_browser_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getppid has no preconditions.
        u32::try_from(unsafe { libc::getppid() }).ok().filter(|pid| *pid > 1)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_pinned_extension_may_start_the_host() {
        assert!(caller_allowed(&format!("chrome-extension://{}/", EXTENSION_IDS[0])));
        assert!(!caller_allowed("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"));
        assert!(!caller_allowed("https://example.com/"));
    }

    #[test]
    fn a_browser_id_cannot_name_a_path() {
        assert!(safe_id("4f1c-9a"));
        assert!(!safe_id("../x"));
        assert!(!safe_id(""));
    }

    #[test]
    fn a_host_removes_only_a_listing_that_still_names_it() {
        let dir = std::env::temp_dir().join(format!("fastbrowse-listing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("b.json");
        let listing = |pid| HostRecord {
            browser_id: "b".into(),
            label: "Chrome".into(),
            browser: "Chrome".into(),
            version: String::new(),
            protocol: PROTOCOL,
            pid,
            browser_pid: None,
            socket: dir.join("b.sock"),
        };
        // A replacement host has rewritten the record: this host leaves it.
        std::fs::write(&record, serde_json::to_vec(&listing(std::process::id() + 1)).unwrap()).unwrap();
        drop(Listing { socket: dir.join("b.sock"), record: record.clone() });
        assert!(record.exists());
        std::fs::write(&record, serde_json::to_vec(&listing(std::process::id())).unwrap()).unwrap();
        drop(Listing { socket: dir.join("b.sock"), record: record.clone() });
        assert!(!record.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn frames_round_trip_and_end_at_eof() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frames(&mut a, r#"{"id":1,"result":{}}"#, 0).await.unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap(), Some(json!({"id": 1, "result": {}})));
        assert_eq!(read_frame(&mut b).await.unwrap(), None);
    }
}
