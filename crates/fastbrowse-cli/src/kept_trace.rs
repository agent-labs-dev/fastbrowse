//! The trace a local run keeps when it ends short of `complete`.
//!
//! Signed-in `--extension` failures on real sites could not be diagnosed: nobody passed `--trace` on the run that
//! failed, and a rerun on a local fixture did not fail the same way (#1414). So the CLI and the MCP server write each
//! run's trace to a file of its own as it goes, delete it when the run completes, and name it in the result otherwise.
//! `serve`, the Python package and the SDKs keep nothing: a server that embeds fastbrowse must not write its users'
//! pages to disk unasked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fastbrowse_core::models::Status;
use fastbrowse_core::py;
use fastbrowse_core::telemetry::{TRACE_ENV, Trace};

/// How long a kept trace stays. Each holds the pages the run read and the screenshots it sent, so they are removed
/// once they are too old to still be the trace of a failure someone is looking into, rather than piling up.
const KEPT_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// One run's trace file, removed unless [`KeptTrace::finish`] keeps it.
pub struct KeptTrace {
    path: PathBuf,
    trace: Trace,
}

impl KeptTrace {
    /// A new trace file in `directory`, or None when it cannot be made: a run is never refused for its trace.
    pub fn start(directory: &Path) -> Option<KeptTrace> {
        remove_old(directory);
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        // Runs in one MCP process can start in the same instant, so a name already taken is tried again.
        let (file, path) = (0..100u32).find_map(|attempt| {
            let tag = &py::sha256_hex(format!("{seed}-{}-{attempt}", std::process::id()).as_bytes())[..8];
            let path = directory.join(format!("{}-{tag}.jsonl", seed / 1_000_000_000));
            private_file(&path).ok().map(|file| (file, path))
        })?;
        Some(KeptTrace { trace: Trace::writing_to(file), path })
    }

    /// What the run writes through: hand it to the run's HTTP client, whose trace every part of the run shares.
    pub fn trace(&self) -> Trace {
        self.trace.clone()
    }

    /// The file's path when the run ended short of `complete` (or with no result at all), else None, the file gone.
    pub fn finish(self, status: Option<Status>) -> Option<PathBuf> {
        if status == Some(Status::Complete) {
            // Windows refuses to delete a file still open, and this is the run's last handle on it.
            drop(self.trace);
            let _ = std::fs::remove_file(&self.path);
            return None;
        }
        Some(self.path)
    }
}

/// Where kept traces go: the per-user state directory under `fastbrowse/traces`, made private, or None when the caller
/// already chose where the trace goes (`--trace`, or `FASTBROWSE_TRACE`, whose `0` keeps none) or it cannot be made.
pub fn directory(flag: Option<&str>, environ: &HashMap<String, String>) -> Option<PathBuf> {
    if flag.or(environ.get(TRACE_ENV).map(String::as_str)).is_some_and(|destination| !destination.is_empty()) {
        return None;
    }
    let home = environ.get("HOME").or_else(|| environ.get("USERPROFILE")).filter(|home| !home.is_empty())?;
    let home = Path::new(home);
    let named = |name: &str| environ.get(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        named("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local"))
    } else {
        named("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"))
    };
    let directory = base.join("fastbrowse/traces");
    std::fs::create_dir_all(&directory).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    Some(directory)
}

/// A new file only its owner can read: a trace holds the pages a signed-in run read.
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Remove kept traces older than [`KEPT_FOR`]. Only `.jsonl` files this module names are touched.
fn remove_old(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age > KEPT_FOR));
        if old && path.extension().is_some_and(|extension| extension == "jsonl") {
            let _ = std::fs::remove_file(path);
        }
    }
}
