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

/// Where a run that is still going keeps its trace, for whoever must name it after dropping the run.
pub type KeptAt = std::sync::Arc<std::sync::Mutex<Option<PathBuf>>>;

/// One run's trace file, removed unless [`KeptTrace::finish`] keeps it.
pub struct KeptTrace {
    path: PathBuf,
    trace: Trace,
}

impl KeptTrace {
    /// A new trace file in `directory`, or None when it cannot be made: a run is never refused for its trace.
    pub fn start(directory: &Path) -> Option<KeptTrace> {
        // Made on every run rather than once: a long-lived MCP server outlives a directory someone removed.
        private_directory(directory).ok()?;
        remove_old(directory);
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        // Runs in one MCP process can start in the same instant, so a name already taken is tried again.
        for attempt in 0..100u32 {
            let tag = &py::sha256_hex(format!("{seed}-{}-{attempt}", std::process::id()).as_bytes())[..8];
            let path = directory.join(format!("{}-{tag}.jsonl", seed / 1_000_000_000));
            match private_file(&path) {
                Ok(file) => return Some(KeptTrace { trace: Trace::writing_to(file), path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// What the run writes through: hand it to the run's HTTP client, whose trace every part of the run shares.
    pub fn trace(&self) -> Trace {
        self.trace.clone()
    }

    /// Where the file is, for a caller that answers before the run ends, such as a cancelled MCP call.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's path when the run ended short of `complete` (or with no result at all), else None, the file gone. A
    /// complete run's file that cannot be removed is named too, rather than left on disk unmentioned.
    pub fn finish(self, status: Option<Status>) -> Option<PathBuf> {
        if status != Some(Status::Complete) {
            return Some(self.path);
        }
        self.trace.close();
        std::fs::remove_file(&self.path).err().map(|_| self.path)
    }
}

/// Where kept traces go: the per-user state directory under `fastbrowse/traces`, or None when the caller already chose
/// where the trace goes (`--trace`, or `FASTBROWSE_TRACE`, whose `0` keeps none) or no home directory is known.
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
    Some(base.join("fastbrowse/traces"))
}

/// `directory`, made if missing, that only its owner can enter: the traces in it hold the pages signed-in runs read.
fn private_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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

/// Remove kept traces older than [`KEPT_FOR`]. Only regular files named as [`KeptTrace::start`] names them are touched,
/// so a file someone else put in the directory is left alone.
fn remove_old(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else { continue };
        let old =
            metadata.modified().is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age > KEPT_FOR));
        if old && metadata.is_file() && entry.file_name().to_str().is_some_and(named_here) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether `name` is one [`KeptTrace::start`] makes: `<seconds>-<eight hex digits>.jsonl`.
fn named_here(name: &str) -> bool {
    let Some((seconds, tag)) = name.strip_suffix(".jsonl").and_then(|stem| stem.split_once('-')) else { return false };
    !seconds.is_empty()
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && tag.len() == 8
        && tag.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_old_traces_this_module_named_are_removed_and_a_removed_directory_is_made_again() {
        let directory = std::env::temp_dir().join(format!("fastbrowse-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let kept = KeptTrace::start(&directory).expect("the directory is made");
        let ours = kept.finish(None).unwrap();
        let theirs = directory.join("notes.jsonl");
        std::fs::write(&theirs, "").unwrap();
        let long_ago = SystemTime::now() - KEPT_FOR - Duration::from_secs(60);
        for path in [&ours, &theirs] {
            std::fs::File::options().write(true).open(path).unwrap().set_modified(long_ago).unwrap();
        }
        remove_old(&directory);
        assert!(!ours.exists() && theirs.exists());
        std::fs::remove_dir_all(&directory).unwrap();
        // A long-lived MCP server keeps its traces after someone removed the directory.
        let again = KeptTrace::start(&directory).expect("made again");
        assert_eq!(again.finish(Some(Status::Complete)), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
