//! What the run, the native host and the extension say to each other.
//!
//! Between Chrome and the host, messages use native messaging's framing: a 4-byte native-endian length, then JSON.
//! Between a run and the host, they are JSON lines on a per-user local socket. The host tags each run's messages
//! with its connection id (`conn`) on the way in and routes by it on the way out, so it reads nothing else.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BrowserError;

/// The relay contract's version. A host or extension speaking another one fails the run before any spend.
pub const PROTOCOL: u32 = 1;

/// The native-messaging host name the extension connects to.
pub const HOST_NAME: &str = "dev.fastbrowse.host";

/// Extension ids allowed to start the host. The first is pinned by the manifest's `key`; its private key is in pass as
/// `dev/FASTBROWSE_EXTENSION_KEY_PEM`, for the first Web Store upload, which keeps the id.
pub const EXTENSION_IDS: [&str; 1] = ["ghodfgfhbfaghhfdabopagbfopdjgpop"];

/// Chrome refuses a message from the host to the extension above 1 MB; larger ones go in chunks.
pub const MAX_TO_EXTENSION: usize = 1024 * 1024;

/// Chrome sends the host messages up to 64 MiB.
pub const MAX_FROM_EXTENSION: usize = 64 * 1024 * 1024;

/// CDP commands the extension relays. Everything else is refused in the extension, so a run can reach the
/// user's profile only through the page: no cookies, storage or saved passwords.
pub const ALLOWED_METHODS: &str = include_str!("../../extension/allowed-methods.json");

/// The extension version that relays every method [`required_methods`] names. The extension reaches users from the
/// Chrome Web Store, on the Store's review schedule, while a fastbrowse release reaches them from PyPI hours after a
/// tag: the two drift unless a release is held, so a run names this version when the browser's copy is older.
pub const MIN_EXTENSION_VERSION: &str = "0.1.1";

/// Methods a run sends best-effort, which an older copy lacks and the run still works without. Each is logged as a
/// warning when the extension does not relay it, never a reason to refuse the run before it starts.
pub const BEST_EFFORT: [&str; 1] = [
    // Only adds the chooser a page opens from a script: the session sends it and runs on when it is refused.
    "Page.setInterceptFileChooserDialog",
];

/// What a connected browser's host writes next to its socket once it is listening, so a run can find it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRecord {
    /// A random id the extension keeps in its storage: one per browser profile.
    pub browser_id: String,
    /// The name the user gave this browser in the extension's popup, or the browser's own name.
    pub label: String,
    /// The browser's product, from its user agent: `Chrome`, `Edge`, `Brave` or `Chromium`.
    pub browser: String,
    pub version: String,
    pub protocol: u32,
    pub pid: u32,
    /// Chrome starts the native host directly; its parent identifies the window for live cursor feedback.
    #[serde(default)]
    pub browser_pid: Option<u32>,
    pub socket: PathBuf,
}

/// Where hosts put their sockets and records: a private per-user directory.
pub fn host_dir() -> PathBuf {
    host_dir_from(|name| std::env::var_os(name), Path::new("/run/user"))
}

fn host_dir_from(
    var: impl Fn(&str) -> Option<OsString>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] run_user: &Path,
) -> PathBuf {
    if let Some(dir) = var("FASTBROWSE_EXTENSION_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "linux")]
    if let Some(runtime) = var("XDG_RUNTIME_DIR").map(PathBuf::from).or_else(|| login_runtime_dir(run_user)) {
        return runtime.join("fastbrowse-hosts");
    }
    let home = var("HOME").or_else(|| var("USERPROFILE")).map(PathBuf::from);
    let home = home.unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/fastbrowse/hosts")
    } else if cfg!(windows) {
        var("LOCALAPPDATA").map_or_else(|| home.join("AppData/Local"), PathBuf::from).join("fastbrowse/hosts")
    } else {
        home.join(".cache/fastbrowse/hosts")
    }
}

/// The user's runtime directory under `run_user`, which systemd-logind creates and sets XDG_RUNTIME_DIR to. The
/// host Chrome starts has the variable, but a shell whose tool keeps only a core environment (Codex's
/// `inherit = "core"`) does not, so a run there read another directory and found no browser connected. Reading
/// the directory itself puts both on the same one. Only a directory the user owns is used, since a host's socket
/// must not land where another user can reach it.
#[cfg(target_os = "linux")]
fn login_runtime_dir(run_user: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = run_user.join(uid.to_string());
    std::fs::metadata(&dir).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid).then_some(dir)
}

/// The CDP methods in [`ALLOWED_METHODS`].
pub fn allowed_methods() -> Vec<String> {
    serde_json::from_str(ALLOWED_METHODS).expect("the extension's allowed-methods.json is a JSON list of strings")
}

/// The CDP methods a run cannot start without: every allowed method except the [`BEST_EFFORT`] ones. An extension
/// that relays these drives any task; a copy that lacks one would refuse a command mid-task, so the run is refused
/// before it spends anything and names what is missing.
pub fn required_methods() -> Vec<String> {
    allowed_methods().into_iter().filter(|method| !BEST_EFFORT.contains(&method.as_str())).collect()
}

/// Split one message for the extension into frames Chrome accepts. A message that fits goes as it is; a larger one
/// becomes `{"chunk": {"id", "index", "count", "data"}}` frames the extension joins before parsing.
pub fn frames_for_extension(message: &str, chunk_id: u64) -> Vec<String> {
    if message.len() <= MAX_TO_EXTENSION {
        return vec![message.to_owned()];
    }
    // Room for the envelope and for JSON-escaping the slice, which at worst doubles a quote or backslash.
    let room = MAX_TO_EXTENSION / 2 - 256;
    let mut pieces = vec![];
    let mut rest = message;
    while !rest.is_empty() {
        let mut end = rest.len().min(room);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    let count = pieces.len();
    pieces
        .into_iter()
        .enumerate()
        .map(|(index, data)| {
            serde_json::json!({"chunk": {"id": chunk_id, "index": index, "count": count, "data": data}}).to_string()
        })
        .collect()
}

/// Every browser whose extension is connected to a running host, read from the records in `dir`.
pub fn connected_hosts(dir: &Path) -> Vec<(PathBuf, HostRecord)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut hosts: Vec<(PathBuf, HostRecord)> = entries
        .flatten()
        .map(|entry| entry.path())
        // A record being written is a dot file until it is renamed into place.
        .filter(|path| {
            path.extension().is_some_and(|x| x == "json")
                && !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .filter_map(|path| {
            let record: HostRecord = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            host_alive(&record).then_some((path, record))
        })
        .collect();
    hosts.sort_by(|a, b| a.1.label.cmp(&b.1.label));
    hosts
}

/// Whether the host that wrote a record still runs. A host that was killed leaves its record behind.
#[cfg(unix)]
fn host_alive(record: &HostRecord) -> bool {
    let Ok(pid) = libc::pid_t::try_from(record.pid) else { return false };
    // SAFETY: signal 0 only asks whether the process exists, and a pid under 1 is refused before it names a group.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// A dead host's pipe is gone with it. Listing the pipes, rather than opening this one, leaves the host's waiting
/// instance free for the run that dials it.
#[cfg(windows)]
fn host_alive(record: &HostRecord) -> bool {
    let Some(name) = record.socket.file_name() else { return false };
    std::fs::read_dir(r"\\.\pipe\").is_ok_and(|pipes| pipes.flatten().any(|pipe| pipe.file_name() == name))
}

/// The connected browser a run uses: the one named `wanted` (its label, browser id or product, any case), or the
/// only one connected. Nothing connected is an outage, so the run ends `unavailable` before any spend.
pub fn pick_host(dir: &Path, wanted: Option<&str>) -> Result<(PathBuf, HostRecord), BrowserError> {
    let mut hosts = connected_hosts(dir);
    let names = |hosts: &[(PathBuf, HostRecord)]| {
        hosts.iter().map(|(_, r)| format!("{} ({} {})", r.label, r.browser, r.version)).collect::<Vec<_>>().join(", ")
    };
    if hosts.is_empty() {
        // Naming the directory shows a run and a host that disagree on it, which a reinstall does not fix.
        return Err(BrowserError::Unavailable(format!(
            "no browser has the fastbrowse extension connected (looked in {}). Run `fastbrowse extension install`, \
             add the extension to Chrome, and keep that Chrome open",
            dir.display()
        )));
    }
    match wanted {
        Some(wanted) => {
            let matches = |r: &HostRecord| {
                [&r.label, &r.browser_id, &r.browser].iter().any(|name| name.eq_ignore_ascii_case(wanted))
            };
            let mut found: Vec<_> = hosts.iter().filter(|(_, r)| matches(r)).cloned().collect();
            match found.len() {
                1 => Ok(found.remove(0)),
                0 => Err(BrowserError::Failed(format!(
                    "no connected browser is named {wanted:?}; connected: {}",
                    names(&hosts)
                ))),
                _ => Err(BrowserError::Failed(format!(
                    "{wanted:?} names more than one connected browser ({}); give each a label in the extension's \
                     popup",
                    names(&found)
                ))),
            }
        }
        None if hosts.len() == 1 => Ok(hosts.remove(0)),
        None => Err(BrowserError::Failed(format!(
            "more than one browser has the fastbrowse extension connected ({}); pick one with --extension-browser",
            names(&hosts)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_message_goes_whole() {
        assert_eq!(frames_for_extension("{\"id\":1}", 7), vec!["{\"id\":1}".to_owned()]);
    }

    #[test]
    fn a_large_message_goes_in_frames_under_chromes_limit_that_join_back_to_it() {
        let script = "a\"\\é".repeat(700_000);
        let message = serde_json::json!({"id": 3, "op": "cdp", "params": {"expression": script}}).to_string();
        let frames = frames_for_extension(&message, 9);
        assert!(frames.len() > 1);
        let mut joined = String::new();
        for (index, frame) in frames.iter().enumerate() {
            assert!(frame.len() <= MAX_TO_EXTENSION, "frame {index} is {} bytes", frame.len());
            let chunk: serde_json::Value = serde_json::from_str(frame).unwrap();
            assert_eq!(chunk["chunk"]["index"], index);
            assert_eq!(chunk["chunk"]["count"], frames.len());
            joined.push_str(chunk["chunk"]["data"].as_str().unwrap());
        }
        assert_eq!(joined, message);
    }

    #[test]
    fn every_cdp_command_the_browser_layer_sends_is_relayed_or_answered_by_the_shim() {
        let allowed = allowed_methods();
        let source_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sent = std::collections::BTreeSet::new();
        let mut stack = vec![source_dir];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (at, _) in text.match_indices(".send(\"").chain(text.match_indices(".call(\"")) {
                        let rest = &text[at..];
                        let start = rest.find('"').unwrap() + 1;
                        let end = start + rest[start..].find('"').unwrap();
                        sent.insert(rest[start..end].to_owned());
                    }
                }
            }
        }
        let domains = super::super::shim::ENABLED_DOMAINS.iter().map(|domain| format!("{domain}.enable"));
        sent.extend(domains);
        let missing: Vec<_> = sent
            .iter()
            .filter(|method| method.contains('.'))
            .filter(|method| !allowed.contains(method) && !super::super::shim::ANSWERED.contains(&method.as_str()))
            .collect();
        assert!(missing.is_empty(), "add to extension/allowed-methods.json or answer in the shim: {missing:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shell_without_xdg_runtime_dir_finds_the_hosts_its_login_session_started() {
        let run_user = std::env::temp_dir().join(format!("fbx-run-user-{}", std::process::id()));
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let runtime = run_user.join(uid.to_string());
        std::fs::create_dir_all(&runtime).unwrap();
        let var = |set: Option<&'static str>| {
            move |name: &str| match name {
                "HOME" => Some(OsString::from("/home/someone")),
                "XDG_RUNTIME_DIR" => set.map(OsString::from),
                _ => None,
            }
        };
        assert_eq!(host_dir_from(var(None), &run_user), runtime.join("fastbrowse-hosts"));
        assert_eq!(host_dir_from(var(Some("/elsewhere")), &run_user), Path::new("/elsewhere/fastbrowse-hosts"));
        std::fs::remove_dir_all(&run_user).unwrap();
        // Without a login session's directory, the user's cache is the only private place left.
        assert_eq!(host_dir_from(var(None), &run_user), Path::new("/home/someone/.cache/fastbrowse/hosts"));
    }

    #[test]
    fn older_host_records_have_no_browser_pid() {
        let old = serde_json::json!({"browserId":"b", "label":"Chrome", "browser":"Chrome",
            "version":"150", "protocol":PROTOCOL, "pid":42, "socket":"b.sock"});
        let record: HostRecord = serde_json::from_value(old).unwrap();
        assert_eq!(record.browser_pid, None);
        let record = HostRecord { browser_pid: Some(7), ..record };
        let roundtrip: HostRecord = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(roundtrip.browser_pid, Some(7));
    }

    fn write_record(dir: &Path, label: &str, pid: u32) {
        let record = HostRecord {
            browser_id: format!("id{label}"),
            label: label.into(),
            browser: "Chrome".into(),
            version: "150".into(),
            protocol: PROTOCOL,
            pid,
            browser_pid: None,
            socket: dir.join(format!("id{label}.sock")),
        };
        std::fs::write(dir.join(format!("id{label}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
    }

    #[test]
    fn a_run_picks_the_only_browser_or_the_one_it_names() {
        let dir = std::env::temp_dir().join(format!("fbx-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let unavailable = pick_host(&dir, None).unwrap_err();
        assert!(unavailable.is_unavailable(), "{unavailable}");
        assert!(unavailable.to_string().contains(&format!("looked in {}", dir.display())), "{unavailable}");
        write_record(&dir, "Work", std::process::id());
        // A record whose host is gone is not a connected browser.
        write_record(&dir, "Gone", u32::MAX - 1);
        std::fs::write(dir.join(".idWork.json.1"), "{").unwrap();
        assert_eq!(pick_host(&dir, None).unwrap().1.label, "Work");
        write_record(&dir, "Home", std::process::id());
        let several = pick_host(&dir, None).unwrap_err().to_string();
        assert!(several.contains("Home (Chrome 150), Work (Chrome 150)"), "{several}");
        assert_eq!(pick_host(&dir, Some("home")).unwrap().1.label, "Home");
        assert!(pick_host(&dir, Some("Gone")).unwrap_err().to_string().contains("no connected browser"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
