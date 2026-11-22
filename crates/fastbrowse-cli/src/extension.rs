//! `fastbrowse native-host` and `fastbrowse extension install | uninstall | status | path`.
//!
//! `install` registers this fastbrowse as the extension's native-messaging host for every Chromium browser it finds:
//! a host manifest per browser, at user level, pointing at a small launcher that runs `<a fastbrowse> native-host`.
//! Chrome starts that launcher when the extension connects. Every fastbrowse that ran `install` stays registered, and
//! the host runs the newest one still present, by the version each binary reports now. `path` writes the extension's own files out for extension
//! development; ordinary installs use the Chrome Web Store.

use std::path::{Path, PathBuf};

// The extension's files are built into the binary, so `path` works offline and always matches this version.
use fastbrowse_browser::extension::FILES;
use fastbrowse_browser::extension::protocol::{EXTENSION_IDS, HOST_NAME};
use serde_json::json;

const STORE_URL: &str = "https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop";

const USAGE: &str = "usage: fastbrowse extension install [--policy] | uninstall | status | path";

/// The file in [`data_dir`] that lists every fastbrowse registered as the host, newest first.
const INSTALLS: &str = "installs.json";

/// The variable the launcher sets to the folder it is in, which holds the registry.
const HOST_DIR: &str = "FASTBROWSE_HOST_DIR";

/// The variable the launcher sets to the registered path it starts, which is the host's identity: a Python install
/// runs inside the interpreter, so the process's own executable says nothing about which install this is.
const HOST_PATH: &str = "FASTBROWSE_HOST_PATH";

/// One fastbrowse that ran `install`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Install {
    path: PathBuf,
    version: String,
}

/// A Chromium browser's per-user configuration folder, relative to the platform's base, by product.
#[cfg(target_os = "linux")]
const BROWSERS: [(&str, &str); 5] = [
    ("Chrome", "google-chrome"),
    ("Chromium", "chromium"),
    ("Edge", "microsoft-edge"),
    ("Brave", "BraveSoftware/Brave-Browser"),
    ("Chrome for Testing", "google-chrome-for-testing"),
];
#[cfg(target_os = "macos")]
const BROWSERS: [(&str, &str); 5] = [
    ("Chrome", "Google/Chrome"),
    ("Chromium", "Chromium"),
    ("Edge", "Microsoft Edge"),
    ("Brave", "BraveSoftware/Brave-Browser"),
    ("Chrome for Testing", "Google/Chrome for Testing"),
];
#[cfg(windows)]
const BROWSERS: [(&str, &str); 4] = [
    ("Chrome", r"Software\Google\Chrome"),
    ("Chromium", r"Software\Chromium"),
    ("Edge", r"Software\Microsoft\Edge"),
    ("Brave", r"Software\BraveSoftware\Brave-Browser"),
];

fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

/// Where fastbrowse keeps the launcher, the host manifest and the unpacked extension.
fn data_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/fastbrowse")
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map_or_else(|| home().join("AppData/Local"), PathBuf::from).join("fastbrowse")
    } else {
        std::env::var_os("XDG_DATA_HOME").map_or_else(|| home().join(".local/share"), PathBuf::from).join("fastbrowse")
    }
}

#[cfg(not(windows))]
fn config_base() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from)
    }
}

/// The command that runs this fastbrowse again: the program as the user invoked it, found on `PATH` when bare, as its
/// canonical path. `install` and `uninstall` both go through here, so one binary has one identity however it is
/// reached (an absolute path, a `PATH` symlink or a shim).
fn this_fastbrowse(program: &str) -> PathBuf {
    let named = Path::new(program);
    let found = if named.components().count() > 1 {
        Some(named.to_path_buf())
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| is_runnable(candidate))
        })
    };
    let found = found.or_else(|| std::env::current_exe().ok()).unwrap_or_else(|| named.to_path_buf());
    canonical(&found)
}

/// A regular file the launcher would run: it skips anything else, so the lookup must too.
fn is_runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A fastbrowse that a package runner unpacked for one call, which the cache may delete later.
fn ephemeral(program: &Path) -> bool {
    let text = program.to_string_lossy();
    ["/.cache/uv/", "/uv/archive-", "/_npx/", "\\uv\\cache\\"].iter().any(|part| text.contains(part))
}

fn host_manifest(launcher: &Path) -> String {
    let origins: Vec<String> = EXTENSION_IDS.iter().map(|id| format!("chrome-extension://{id}/")).collect();
    let manifest = json!({
        "name": HOST_NAME,
        "description": "fastbrowse: lets runs reach this browser through the fastbrowse extension",
        "path": launcher,
        "type": "stdio",
        "allowed_origins": origins,
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_default()
}

/// The registered installs; none when `install` has not run since installs were recorded. A file that cannot be read
/// or parsed is an error, never an empty list: an install or uninstall that took it for empty would forget every
/// other install and, from `uninstall`, delete the host they still need.
fn read_installs(dir: &Path) -> Result<Vec<Install>, String> {
    let path = dir.join(INSTALLS);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        format!("cannot parse {}: {e}; fix or delete it, then run `fastbrowse extension install`", path.display())
    })
}

/// What a fastbrowse reports for `--version`: its version, as "fastbrowse 0.13.0 (sha)" says it.
fn reported_version(program: &Path) -> Option<String> {
    reported_version_within(program, VERSION_PROBE_LIMIT)
}

/// How long a `--version` probe may run before it is a hung process: it only prints a line.
const VERSION_PROBE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// [`reported_version`] bounded as a whole by `limit`, the exit and the read of its output alike: a wrapper that exits
/// but leaves a helper holding stdout open would otherwise block the read. A probe past the limit is killed and its
/// reader abandoned, so a hung binary cannot hold up the rest.
fn reported_version_within(program: &Path, limit: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![];
        let _ = stdout.read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let started = std::time::Instant::now();
    let bytes = receiver.recv_timeout(limit).ok();
    // Only a probe that exits 0 counts: a wrapper that prints a version and fails is not a working install.
    let exited_ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < limit => std::thread::sleep(std::time::Duration::from_millis(5)),
            _ => break false,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let text = String::from_utf8(bytes.filter(|_| exited_ok)?).ok()?;
    let version = text.split_whitespace().nth(1)?;
    version.starts_with(|c: char| c.is_ascii_digit()).then(|| version.to_string())
}

/// What each registered install in `dir` (and `this`) reports now, probed before the registry's lock is taken so a slow
/// probe never holds it. An install registered after this read keeps the version recorded for it.
fn probe_registered(dir: &Path, this: &Path) -> impl Fn(&Path) -> Option<String> + use<> {
    let mut paths: Vec<PathBuf> =
        read_installs(dir).unwrap_or_default().into_iter().map(|install| install.path).collect();
    paths.push(this.to_path_buf());
    paths.sort();
    paths.dedup();
    let probes: Vec<_> =
        paths.into_iter().map(|path| std::thread::spawn(move || (path.clone(), reported_version(&path)))).collect();
    let versions: std::collections::HashMap<PathBuf, Option<String>> =
        probes.into_iter().filter_map(|probe| probe.join().ok()).collect();
    move |path: &Path| versions.get(path).cloned().flatten()
}

/// `installs` with each version replaced by what the binary reports now, where it answers: an install upgraded in
/// place keeps its path, so the version recorded when it registered goes stale. Newest first.
fn refreshed(installs: Vec<Install>, query: &dyn Fn(&Path) -> Option<String>) -> Vec<Install> {
    let mut installs: Vec<Install> = installs
        .into_iter()
        .map(|install| Install { version: query(&install.path).unwrap_or(install.version), path: install.path })
        .collect();
    installs.sort_by_key(|install| std::cmp::Reverse(precedence(&install.version)));
    installs
}

/// The registered installs whose program is still there, newest first by the version each reports now.
fn present_installs(dir: &Path, query: &dyn Fn(&Path) -> Option<String>) -> Result<Vec<Install>, String> {
    Ok(refreshed(read_installs(dir)?.into_iter().filter(|install| install.path.is_file()).collect(), query))
}

/// One dot-separated pre-release identifier: numbers rank below words, as semver orders them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u64),
    Word(String),
}

/// A version's precedence as semver orders it: the numeric core, then a release above its pre-releases, then the
/// pre-release identifiers one by one (`rc.2` above `rc.1`, `rc.1` above `beta.11`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Precedence {
    core: Vec<u64>,
    release: bool,
    pre: Vec<Identifier>,
}

fn precedence(version: &str) -> Precedence {
    let version = version.split('+').next().unwrap_or_default();
    let (core, pre) = version.split_once('-').map_or((version, None), |(core, pre)| (core, Some(pre)));
    let mut core: Vec<u64> = core.split('.').map(|part| part.parse().unwrap_or(0)).collect();
    core.resize(core.len().max(3), 0);
    let pre = pre.map_or_else(Vec::new, |pre| {
        pre.split('.')
            .map(|part| part.parse().map_or_else(|_| Identifier::Word(part.to_string()), Identifier::Number))
            .collect()
    });
    Precedence { core, release: pre.is_empty(), pre }
}

/// `installs` with `this` added, or its earlier entry replaced, less any whose program is gone, newest first.
///
/// An app that bundles its own fastbrowse and the user's own install each run `install`, and each used to replace
/// the other's launcher, so whichever ran last served every run and a version mismatch failed runs from the other
/// (#1456). Both stay registered now, and the newest serves: the Store updates the extension to match the newest
/// fastbrowse, and an older one still serves once the newer is removed. A tie keeps the earlier registration first.
fn with_install(installs: Vec<Install>, this: Install) -> Vec<Install> {
    let mut installs: Vec<Install> =
        installs.into_iter().filter(|install| install.path != this.path && install.path.is_file()).collect();
    installs.push(this);
    installs.sort_by_key(|install| std::cmp::Reverse(precedence(&install.version)));
    installs
}

/// The registered installs, one per line, in the order the host tries them.
fn describe(installs: &[Install]) -> String {
    installs.iter().map(|install| format!("  {} (fastbrowse {})\n", install.path.display(), install.version)).collect()
}

/// Write `bytes` to `path` as one rename, so a reader or a crash never sees half a file.
fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> std::io::Result<()> {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let written = std::fs::write(&temporary, bytes).and_then(|()| {
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
        }
        let _ = executable;
        std::fs::rename(&temporary, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Run `work` holding the registry's lock, which every read-modify-write of the installs and the files derived from
/// them takes, so two installs at once (an app's postinstall and the user's own) each see the other's entry. The
/// operating system drops the lock if the holder dies.
fn locked<T>(dir: &Path, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("installs.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    lock.lock().map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    work()
}

/// Record `installs`, then write the launcher that runs the first of them still present. The registry goes first: it
/// is the source of truth the host reads when it hands over, and the launcher is derived from it, so a write that is
/// interrupted between the two leaves a launcher that is stale, never a registry that disagrees with what was asked.
fn write_host(dir: &Path, installs: &[Install]) -> Result<PathBuf, String> {
    let path = dir.join(INSTALLS);
    let text = serde_json::to_string_pretty(installs).map_err(|e| format!("cannot record the installs: {e}"))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    write_atomic(&path, text.as_bytes(), false).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let commands: Vec<PathBuf> = installs.iter().map(|install| install.path.clone()).collect();
    write_launcher(dir, &commands).map_err(|e| format!("cannot write the launcher: {e}"))
}

/// A launcher that runs the first of `commands` that is a regular, executable file as the host (on Unix it
/// `exec`s, so Chrome stays the host's parent). A registered install that cannot start, as a script whose interpreter was
/// removed, is not skipped: uninstall or reinstall it, which rewrites the launcher. There is no fallback
/// to a `fastbrowse` on the `PATH` Chrome starts it with: that is a binary nobody registered, and an explicit
/// `uninstall` must stay uninstalled. With none left it says so and fails. A newer registered install than the one
/// it runs takes over inside `fastbrowse native-host` (see [`delegate`]). The launcher tells it where the registry
/// is, by its own location, because Chrome's environment can differ from the one `install` ran in.
fn write_launcher(dir: &Path, commands: &[PathBuf]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    const NONE: &str = "fastbrowse: no registered install is present; run `fastbrowse extension install`";
    if cfg!(windows) {
        let launcher = dir.join("native-host.bat");
        let mut text = format!("@echo off\r\nset \"{HOST_DIR}=%~dp0\"\r\n");
        for command in commands {
            // In a batch file a `%` in a path would start a variable, so each is doubled. `name\` exists only for a folder.
            let command = command.display().to_string().replace('%', "%%");
            text.push_str(&format!(
                "if exist \"{command}\" if not exist \"{command}\\\" (\r\n  set \"{HOST_PATH}={command}\"\r\n  \"{command}\" native-host %*\r\n  exit /b\r\n)\r\n"
            ));
        }
        text.push_str(&format!("echo {NONE} 1>&2\r\nexit /b 1\r\n"));
        write_atomic(&launcher, text.as_bytes(), false)?;
        return Ok(launcher);
    }
    let launcher = dir.join("native-host");
    let quoted: Vec<String> =
        commands.iter().map(|command| format!("'{}'", command.display().to_string().replace('\'', r"'\''"))).collect();
    let text = format!(
        "#!/bin/sh\n{HOST_DIR}=$(dirname \"$0\"); export {HOST_DIR}\nfor fastbrowse in {}; do\n    if [ -f \"$fastbrowse\" ] && [ -x \"$fastbrowse\" ]; then {HOST_PATH}=$fastbrowse; export {HOST_PATH}; exec \"$fastbrowse\" native-host \"$@\"; fi\ndone\necho '{NONE}' >&2\nexit 127\n",
        quoted.join(" ")
    );
    write_atomic(&launcher, text.as_bytes(), true)?;
    Ok(launcher)
}

/// Every other install in `installs` with the version it reports now. An install that does not answer `--version`
/// cannot be started, so it is left out: a broken copy must not shadow a working one.
fn probe_others(installs: &[Install], own: &Path, query: &dyn Fn(&Path) -> Option<String>) -> Vec<Install> {
    let own = canonical(own);
    installs
        .iter()
        .filter(|install| canonical(&install.path) != own)
        .filter_map(|install| Some(Install { version: query(&install.path)?, path: install.path.clone() }))
        .collect()
}

/// The probed installs still in `installs` this host should hand over to, newest first: the newer ones when this host
/// is registered, every other one when it is not (a stray copy serving through the launcher).
fn handover_candidates(installs: &[Install], probed: Vec<Install>, own: &Path, own_version: &str) -> Vec<Install> {
    let own = canonical(own);
    let registered = is_registered(installs, &own);
    let mut newer: Vec<Install> = probed
        .into_iter()
        .filter(|probe| installs.iter().any(|install| canonical(&install.path) == canonical(&probe.path)))
        .filter(|probe| !registered || precedence(&probe.version) > precedence(own_version))
        .collect();
    newer.sort_by_key(|install| std::cmp::Reverse(precedence(&install.version)));
    newer
}

#[cfg(test)]
fn newer_installs(
    installs: &[Install],
    own: &Path,
    own_version: &str,
    query: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Install> {
    handover_candidates(installs, probe_others(installs, own, query), own, own_version)
}

fn is_registered(installs: &[Install], own: &Path) -> bool {
    let own = canonical(own);
    installs.iter().any(|install| canonical(&install.path) == own)
}

/// What a host does when no install took over: a registered one serves itself; an unregistered one (a stray copy that
/// the launcher or a stale manifest reached) must not serve, and says where the registry is.
fn without_handover(registered: bool, dir: &Path) -> Option<i32> {
    if registered {
        return None;
    }
    eprintln!(
        "fastbrowse native-host: this fastbrowse is not registered and no registered install started; see {} or run `fastbrowse extension install`",
        dir.join(INSTALLS).display()
    );
    Some(1)
}

/// Whether `own` is registered and the installs to hand over to first; None when there is no registry (the caller
/// decides whether that serves). A registry that cannot be read is an error.
fn plan(
    dir: &Path,
    own: &Path,
    own_version: &str,
    delegated: bool,
    query: &dyn Fn(&Path) -> Option<String>,
) -> Result<Option<(bool, Vec<Install>)>, String> {
    match std::fs::metadata(dir.join(INSTALLS)) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.join(INSTALLS).display())),
    }
    let installs = read_installs(dir)?;
    // A host another one handed over to serves only if it is registered itself, and hands over no further: it probes nothing.
    let probed = if delegated { Vec::new() } else { probe_others(&installs, own, query) };
    // Probing took time: an uninstall may have landed meanwhile, so whether this host is registered and who it hands
    // over to are both decided from the registry as it is now, with the probe results already taken.
    let installs = read_installs(dir)?;
    let candidates = handover_candidates(&installs, probed, own, own_version);
    Ok(Some((is_registered(&installs, own), candidates)))
}

/// The folder holding the registry: the launcher's, when Chrome started this through it.
fn host_dir() -> PathBuf {
    std::env::var_os(HOST_DIR).map_or_else(data_dir, PathBuf::from)
}

/// The command that runs `install` as the host in this one's place, with Chrome's arguments.
fn handover_command(install: &Install, args: &[String]) -> std::process::Command {
    let mut command = std::process::Command::new(&install.path);
    command.arg("native-host").args(args).env(GUARD, "1").env(HOST_PATH, &install.path);
    command
}

const GUARD: &str = "FASTBROWSE_HOST_DELEGATED";

/// Run the newest registered fastbrowse as the host instead of this one, when one is newer; its exit code. Newer
/// installs are tried newest first until one starts. `None` runs this one: it is registered and the newest (or no newer
/// one starts), or there is no registry and no launcher named this host. An unregistered one, or an unreadable registry, exits 1 instead of serving
/// (see [`without_handover`]).
pub fn delegate(args: &[String]) -> Option<i32> {
    let delegated = std::env::var_os(GUARD).is_some();
    let dir = host_dir();
    // The identity is the path the launcher started; run directly, without a launcher, it is this executable.
    let own = std::env::var_os(HOST_PATH).map(PathBuf::from).or_else(|| std::env::current_exe().ok())?;
    let (registered, candidates) = match plan(&dir, &own, env!("CARGO_PKG_VERSION"), delegated, &reported_version) {
        Ok(Some(plan)) => plan,
        // No registry: a launcher from before installs were recorded serves as it always did. A launcher that knows
        // the registry (it sets the path) and finds it gone was started just before the last uninstall: it must not serve.
        Ok(None) if std::env::var_os(HOST_PATH).is_none() => return None,
        Ok(None) => return without_handover(false, &dir),
        Err(error) => {
            eprintln!("fastbrowse native-host: {error}");
            return Some(1);
        }
    };
    for newer in candidates {
        let mut command = handover_command(&newer, args);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = command.exec();
            eprintln!("fastbrowse native-host: cannot start {}: {error}", newer.path.display());
        }
        #[cfg(not(unix))]
        match command.status() {
            Ok(status) => return Some(status.code().unwrap_or(1)),
            Err(error) => eprintln!("fastbrowse native-host: cannot start {}: {error}", newer.path.display()),
        }
    }
    without_handover(registered, &dir)
}

/// Register the host with every browser found, keeping the other registered installs; the names of the browsers
/// registered.
fn install(program: &str, out: &mut String) -> Result<Vec<&'static str>, String> {
    let fastbrowse = this_fastbrowse(program);
    if ephemeral(&fastbrowse) {
        out.push_str(&format!(
            "warning: {} looks like a one-off copy from a package runner's cache, which may be deleted. Install \
             fastbrowse (for example `uv tool install fastbrowse`) and run this again from it.\n",
            fastbrowse.display()
        ));
    }
    let this = Install { path: fastbrowse, version: env!("CARGO_PKG_VERSION").into() };
    let dir = data_dir();
    let query = probe_registered(&dir, &this.path);
    record_install(&dir, this, &query, out, register)
}

/// Add `this` to the registry in `dir` and write the launcher and host manifest, then `register` the manifest with
/// the browsers, all under the registry's lock.
fn record_install(
    dir: &Path,
    this: Install,
    query: &dyn Fn(&Path) -> Option<String>,
    out: &mut String,
    register: impl FnOnce(&Path, &str) -> Result<Vec<&'static str>, String>,
) -> Result<Vec<&'static str>, String> {
    locked(dir, || {
        let before = present_installs(dir, query)?;
        let installs = with_install(before.clone(), this.clone());
        let owner = &installs[0];
        // Say when the host Chrome starts changes hands, or stays with another fastbrowse, so neither install's owner
        // is surprised by which version serves their runs.
        match before.first() {
            Some(previous) if previous.path != this.path && owner.path == this.path => out.push_str(&format!(
                "This fastbrowse replaces {} (fastbrowse {}) as the host the extension starts; that one stays \
                 registered and serves again if this one is removed.\n",
                previous.path.display(),
                previous.version
            )),
            _ if owner.path != this.path && precedence(&owner.version) > precedence(&this.version) => {
                out.push_str(&format!(
                    "{} (fastbrowse {}) is newer and stays the host the extension starts; this fastbrowse is \
                     registered and serves if that one is removed.\n",
                    owner.path.display(),
                    owner.version
                ))
            }
            _ if owner.path != this.path => out.push_str(&format!(
                "{} (fastbrowse {}) is the same version and stays the host the extension starts; this fastbrowse is \
                 registered and serves if that one is removed.\n",
                owner.path.display(),
                owner.version
            )),
            _ => {}
        }
        out.push_str(&format!(
            "The extension's host runs the first of these still installed:\n{}",
            describe(&installs)
        ));
        let launcher = write_host(dir, &installs)?;
        let manifest = host_manifest(&launcher);
        let manifest_path = dir.join(format!("{HOST_NAME}.json"));
        write_atomic(&manifest_path, manifest.as_bytes(), false)
            .map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
        register(&manifest_path, &manifest)
    })
}

#[cfg(not(windows))]
fn register(_manifest_path: &Path, manifest: &str) -> Result<Vec<&'static str>, String> {
    let mut registered = vec![];
    for (name, folder) in BROWSERS {
        let profile_root = config_base().join(folder);
        if !profile_root.is_dir() {
            continue;
        }
        let hosts = profile_root.join("NativeMessagingHosts");
        std::fs::create_dir_all(&hosts).map_err(|e| format!("cannot create {}: {e}", hosts.display()))?;
        let path = hosts.join(format!("{HOST_NAME}.json"));
        write_atomic(&path, manifest.as_bytes(), false).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        registered.push(name);
    }
    Ok(registered)
}

#[cfg(windows)]
fn register(manifest_path: &Path, _manifest: &str) -> Result<Vec<&'static str>, String> {
    let mut registered = vec![];
    for (name, key) in BROWSERS {
        let key = format!(r"HKCU\{key}\NativeMessagingHosts\{HOST_NAME}");
        let status = std::process::Command::new("reg")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
            .arg(manifest_path)
            .arg("/f")
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("cannot run reg: {e}"))?;
        if status.success() {
            registered.push(name);
        }
    }
    Ok(registered)
}

/// What `uninstall` did.
#[derive(Debug, PartialEq)]
enum Uninstalled {
    /// Other installs are still registered, so the host stays, running them.
    Kept(Vec<Install>),
    /// This fastbrowse is not registered and others are: the registry stays as it was, and the launcher is rewritten from it.
    NotRegistered(Vec<Install>),
    /// This was the last registered install: the browsers the host was removed from.
    Removed(Vec<&'static str>),
}

/// Unregister this fastbrowse. The host stays while another install is registered, so an app removing its bundled
/// copy does not take the user's own install with it; the last one out removes exactly what `install` wrote.
fn uninstall(program: &str) -> Result<Uninstalled, String> {
    let (dir, this) = (data_dir(), this_fastbrowse(program));
    let query = probe_registered(&dir, &this);
    unregister(&dir, &this, &query, remove_browsers)
}

fn unregister(
    dir: &Path,
    this: &Path,
    query: &dyn Fn(&Path) -> Option<String>,
    remove_browsers: impl FnOnce() -> Vec<&'static str>,
) -> Result<Uninstalled, String> {
    locked(dir, || {
        let all = read_installs(dir)?;
        let registered = all.iter().any(|install| canonical(&install.path) == this);
        let rest = refreshed(
            all.into_iter().filter(|install| canonical(&install.path) != this && install.path.is_file()).collect(),
            query,
        );
        if !rest.is_empty() {
            // Rewritten even when this was never registered, so a retry after an interrupted uninstall repairs the
            // launcher the registry has already moved past.
            write_host(dir, &rest)?;
            return Ok(if registered { Uninstalled::Kept(rest) } else { Uninstalled::NotRegistered(rest) });
        }
        // The registry goes first and its failure is the uninstall's: nothing else is removed, so a retry finishes it.
        match std::fs::remove_file(dir.join(INSTALLS)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", dir.join(INSTALLS).display())),
        }
        let removed = remove_browsers();
        for file in ["native-host", "native-host.bat", &format!("{HOST_NAME}.json")] {
            let _ = std::fs::remove_file(dir.join(file));
        }
        Ok(Uninstalled::Removed(removed))
    })
}

#[cfg(not(windows))]
fn remove_browsers() -> Vec<&'static str> {
    let mut removed = vec![];
    for (name, folder) in BROWSERS {
        let path = config_base().join(folder).join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
        if std::fs::remove_file(path).is_ok() {
            removed.push(name);
        }
    }
    removed
}

#[cfg(windows)]
fn remove_browsers() -> Vec<&'static str> {
    let mut removed = vec![];
    for (name, key) in BROWSERS {
        let key = format!(r"HKCU\{key}\NativeMessagingHosts\{HOST_NAME}");
        let status = std::process::Command::new("reg").args(["delete", &key, "/f"]).status();
        if status.is_ok_and(|s| s.success()) {
            removed.push(name);
        }
    }
    removed
}

/// Write the extension's files to a folder for Load unpacked, and return it.
fn unpacked() -> Result<PathBuf, String> {
    let dir = data_dir().join("extension");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (name, text) in FILES {
        std::fs::write(dir.join(name), text).map_err(|e| format!("cannot write {name}: {e}"))?;
    }
    Ok(dir)
}

const POLICY: &str = "/etc/opt/chrome/policies/managed/fastbrowse.json";

fn policy_text() -> String {
    let entry = format!("{};https://clients2.google.com/service/update2/crx", EXTENSION_IDS[0]);
    serde_json::to_string_pretty(&json!({"ExtensionInstallForcelist": [entry]})).unwrap_or_default()
}

/// `fastbrowse extension ...`: its output and exit code.
pub fn command(program: &str, args: &[String]) -> (String, i32) {
    let mut out = String::new();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["install"] | ["install", "--policy"] => {
            let registered = match install(program, &mut out) {
                Ok(registered) => registered,
                Err(error) => return (format!("{out}fastbrowse: {error}\n"), 1),
            };
            if registered.is_empty() {
                out.push_str(
                    "No Chrome, Chromium, Edge or Brave profile was found for this user. Open the browser \
                              once, then run this again.\n",
                );
                return (out, 1);
            }
            out.push_str(&format!("Registered the fastbrowse host with {}.\n", registered.join(", ")));
            out.push_str(
                "Requires extension 0.1.1 or newer with the required commands. If the Store update is not available, \
                 run `fastbrowse extension path`, then enable Developer mode at chrome://extensions and choose \
                 Load unpacked with the printed folder.\n",
            );
            if args.len() == 2 {
                out.push_str(&format!(
                    "\nTo install the extension by policy, with no prompt and no debugging bar, write this to {POLICY} \
                     as root (Linux; the browser will then say it is managed by your organization):\n\n{}\n",
                    policy_text()
                ));
            } else {
                out.push_str(&format!(
                    "Add fastbrowse to Chrome: {STORE_URL}\nOpen it from Chrome's Extensions menu to see runs and stop them.\n\n\
                     Then run a task: fastbrowse \"What is the title of the top story?\" \
                     --start https://news.ycombinator.com/ --extension --browser\n"
                ));
            }
            (out, 0)
        }
        ["uninstall"] => match uninstall(program) {
            Ok(Uninstalled::Kept(rest)) => (
                format!(
                    "Unregistered this fastbrowse. The extension's host stays registered and runs the first of these \
                     still installed:\n{}Run `uninstall` from each of them to remove the host.\n",
                    describe(&rest)
                ),
                0,
            ),
            Ok(Uninstalled::NotRegistered(rest)) => (
                format!(
                    "This fastbrowse was not registered, so nothing changed. The extension's host runs the first of \
                     these still installed:\n{}",
                    describe(&rest)
                ),
                0,
            ),
            Ok(Uninstalled::Removed(removed)) if removed.is_empty() => {
                ("Nothing to remove: the fastbrowse host was not registered.\n".into(), 0)
            }
            Ok(Uninstalled::Removed(removed)) => {
                (format!("Removed the fastbrowse host from {}.\n", removed.join(", ")), 0)
            }
            Err(error) => (format!("fastbrowse: {error}\n"), 1),
        },
        ["status"] => {
            let installs = match present_installs(&data_dir(), &reported_version) {
                Ok(installs) => installs,
                Err(error) => return (format!("fastbrowse: {error}\n"), 1),
            };
            if installs.is_empty() {
                ("No fastbrowse is registered as the extension's host. Run `fastbrowse extension install`.\n".into(), 1)
            } else {
                (format!("The extension's host runs the first of these still installed:\n{}", describe(&installs)), 0)
            }
        }
        ["path"] => match unpacked() {
            Ok(dir) => (format!("{}\n", dir.display()), 0),
            Err(error) => (format!("fastbrowse: {error}\n"), 1),
        },
        _ => (format!("{USAGE}\n"), 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_manifest_names_the_launcher_and_only_our_extension() {
        let manifest: serde_json::Value = serde_json::from_str(&host_manifest(Path::new("/x/native-host"))).unwrap();
        assert_eq!(manifest["name"], HOST_NAME);
        assert_eq!(manifest["type"], "stdio");
        assert_eq!(manifest["path"], "/x/native-host");
        assert_eq!(manifest["allowed_origins"], json!([format!("chrome-extension://{}/", EXTENSION_IDS[0])]));
    }

    #[test]
    fn a_package_runners_cache_copy_is_flagged() {
        assert!(ephemeral(Path::new("/home/u/.cache/uv/archive-v0/abc/bin/fastbrowse")));
        assert!(!ephemeral(Path::new("/home/u/.local/bin/fastbrowse")));
    }

    #[test]
    fn the_built_in_manifest_pins_the_id_the_host_allows() {
        let manifest: serde_json::Value = serde_json::from_slice(FILES[0].1).unwrap();
        let key = manifest["key"].as_str().unwrap();
        let der = base64_decode(key);
        let digest = sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&der));
        let id: String = digest[..16]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 15])
            .map(|nibble| char::from(b'a' + nibble))
            .collect();
        assert_eq!(id, EXTENSION_IDS[0]);
    }

    #[test]
    fn every_file_in_the_extension_folder_is_built_in() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fastbrowse-browser/extension");
        let mut on_disk: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        let mut built_in: Vec<String> = FILES.iter().map(|(name, _)| name.to_string()).collect();
        on_disk.sort();
        built_in.sort();
        assert_eq!(on_disk, built_in, "add the file to fastbrowse_browser::extension::FILES");
    }

    fn base64_decode(text: &str) -> Vec<u8> {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = 0u32;
        let mut count = 0;
        let mut out = vec![];
        for byte in text.bytes().filter(|b| *b != b'=') {
            bits = (bits << 6) | alphabet.iter().position(|a| *a == byte).unwrap() as u32;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_runs_this_fastbrowse_as_the_host() {
        let dir = scratch("launcher-text");
        let launcher = write_launcher(&dir, &[PathBuf::from("/opt/it's/fastbrowse")]).unwrap();
        let text = std::fs::read_to_string(&launcher).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        if cfg!(windows) {
            assert!(text.contains("native-host %*"));
        } else {
            assert!(text.contains("for fastbrowse in '/opt/it'\\''s/fastbrowse'; do"), "{text}");
        }
    }

    #[test]
    fn versions_order_as_semver_does() {
        assert!(precedence("0.12.0") > precedence("0.11.9"));
        assert!(precedence("0.10.0") > precedence("0.9.1"));
        assert!(precedence("0.12.0") > precedence("0.12.0-rc.1"));
        assert_eq!(precedence("0.12.0+local"), precedence("0.12.0"));
    }

    #[test]
    fn prereleases_order_by_their_identifiers() {
        assert!(precedence("1.0.0-rc.2") > precedence("1.0.0-rc.1"));
        assert!(precedence("1.0.0-rc.10") > precedence("1.0.0-rc.2"));
        assert!(precedence("1.0.0-rc.1") > precedence("1.0.0-beta.11"));
        assert!(precedence("1.0.0-alpha.1") > precedence("1.0.0-alpha"));
        assert!(precedence("1.0.0-alpha.beta") > precedence("1.0.0-alpha.1"));
        assert!(precedence("1.0.0") > precedence("1.0.0-rc.9"));
    }

    #[cfg(unix)]
    fn script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fastbrowse-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(path: &Path, version: &str) -> Install {
        Install { path: path.to_path_buf(), version: version.into() }
    }

    #[test]
    fn an_unreadable_registry_is_an_error_not_an_empty_one() {
        let dir = scratch("registry");
        assert_eq!(read_installs(&dir).unwrap(), vec![]);
        std::fs::write(dir.join(INSTALLS), "[{\"path\": \"/x\", \"ver").unwrap();
        let error = read_installs(&dir).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(error.contains("cannot parse"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupt_registry_stops_install_and_uninstall_without_touching_anything() {
        let dir = scratch("corrupt");
        let program = dir.join("bin/fastbrowse");
        script(&program, "echo fastbrowse 1.0.0");
        std::fs::write(dir.join(INSTALLS), "not json").unwrap();
        std::fs::write(dir.join("native-host"), "keep").unwrap();
        let query = |_: &Path| None;
        let mut out = String::new();
        let installed = record_install(&dir, at(&program, "1.0.0"), &query, &mut out, |_, _| Ok(vec!["Chrome"]));
        let removed = unregister(&dir, &canonical(&program), &query, || vec!["Chrome"]);
        let kept = std::fs::read_to_string(dir.join("native-host")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(installed.is_err() && removed.is_err());
        assert_eq!(kept, "keep");
    }

    #[test]
    fn installs_at_once_each_keep_their_entry() {
        let dir = scratch("concurrent");
        let programs: Vec<PathBuf> = (0..8).map(|n| dir.join(format!("fastbrowse{n}"))).collect();
        for program in &programs {
            std::fs::write(program, "x").unwrap();
        }
        std::thread::scope(|scope| {
            for program in &programs {
                let dir = &dir;
                scope.spawn(move || {
                    // A register step that dawdles gives an unlocked read-modify-write time to lose entries.
                    let register = |_: &Path, _: &str| {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok(vec!["Chrome"])
                    };
                    let mut out = String::new();
                    record_install(dir, at(program, "1.0.0"), &|_: &Path| None, &mut out, register).unwrap();
                });
            }
        });
        let count = read_installs(&dir).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(count, 8);
    }

    #[cfg(unix)]
    #[test]
    fn the_launcher_skips_a_folder_and_never_runs_an_unregistered_fastbrowse_from_the_path() {
        let dir = scratch("launcher");
        let stray = dir.join("path/fastbrowse");
        script(&stray, "echo stray");
        let folder = dir.join("gone/fastbrowse");
        std::fs::create_dir_all(&folder).unwrap();
        let run = |commands: &[PathBuf]| {
            let launcher = write_launcher(&dir, commands).unwrap();
            std::process::Command::new(launcher)
                .env("PATH", format!("{}:/usr/bin:/bin", stray.parent().unwrap().display()))
                .output()
                .unwrap()
        };
        let only_a_folder = run(std::slice::from_ref(&folder));
        let real = dir.join("real/fastbrowse");
        script(&real, "echo real \"$@\" $FASTBROWSE_HOST_DIR");
        let after_a_folder = run(&[folder, real]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!only_a_folder.status.success());
        assert_eq!(String::from_utf8_lossy(&only_a_folder.stdout), "");
        assert!(String::from_utf8_lossy(&only_a_folder.stderr).contains("extension install"));
        // The launcher says where it lives, which is where the registry is.
        assert_eq!(String::from_utf8_lossy(&after_a_folder.stdout), format!("real native-host {}\n", dir.display()));
    }

    #[cfg(unix)]
    #[test]
    fn an_unregistered_host_hands_over_to_the_newest_install_even_when_older() {
        let dir = scratch("stray");
        let (stray, old) = (dir.join("stray/fastbrowse"), dir.join("old/fastbrowse"));
        script(&stray, "echo fastbrowse 0.16.0");
        script(&old, "echo fastbrowse 0.15.0");
        let installs = vec![at(&old, "0.15.0")];
        let candidates = newer_installs(&installs, &stray, "0.16.0", &reported_version);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(candidates, vec![at(&old, "0.15.0")]);
    }

    #[test]
    fn an_unregistered_host_that_no_install_took_over_from_exits_instead_of_serving() {
        let dir = scratch("unserved");
        let (known, stray) = (dir.join("a/fastbrowse"), dir.join("stray/fastbrowse"));
        let installs = vec![at(&known, "0.15.0")];
        assert!(is_registered(&installs, &known));
        assert!(!is_registered(&installs, &stray));
        let registered_serves = without_handover(true, &dir);
        let stray_exits = without_handover(false, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(registered_serves, None);
        assert_eq!(stray_exits, Some(1));
    }

    #[test]
    fn the_plan_serves_without_a_registry_and_fails_on_a_corrupt_one_and_keys_on_the_started_path() {
        let dir = scratch("plan");
        let (started, other) = (dir.join("tool/bin/fastbrowse"), dir.join("other/fastbrowse"));
        let none = |_: &Path| None;
        // No registry: a launcher from before installs were recorded.
        assert_eq!(plan(&dir, &started, "0.15.0", false, &none), Ok(None));
        // A Python install: the started script is registered whatever executable runs it.
        let installs = vec![at(&started, "0.15.0")];
        write_host(&dir, &installs).unwrap();
        let Ok(Some((registered, candidates))) = plan(&dir, &started, "0.15.0", false, &none) else {
            panic!("no plan")
        };
        assert!(registered && candidates.is_empty());
        let Ok(Some((registered, _))) = plan(&dir, &other, "0.15.0", false, &none) else { panic!("no plan") };
        assert!(!registered);
        std::fs::write(dir.join(INSTALLS), "{").unwrap();
        let corrupt = plan(&dir, &started, "0.15.0", false, &none);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(corrupt.unwrap_err().contains("installs.json"));
    }

    #[cfg(unix)]
    #[test]
    fn the_launcher_tells_the_host_which_registered_path_it_started() {
        let dir = scratch("identity-env");
        let real = dir.join("real/fastbrowse");
        script(&real, "echo $FASTBROWSE_HOST_PATH");
        let launcher = write_launcher(&dir, std::slice::from_ref(&real)).unwrap();
        let out = std::process::Command::new(launcher).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{}\n", real.display()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_and_its_target_are_one_identity() {
        let dir = scratch("identity");
        let program = dir.join("real/fastbrowse");
        script(&program, "true");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&program, &link).unwrap();
        let via_link = this_fastbrowse(link.to_str().unwrap());
        let via_path = this_fastbrowse(program.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(via_link, via_path);
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_counts_only_when_the_child_exits_zero() {
        let dir = scratch("exit-status");
        let (bad, good) = (dir.join("bad/fastbrowse"), dir.join("good/fastbrowse"));
        script(&bad, "echo fastbrowse 0.16.0\nexit 1");
        script(&good, "echo fastbrowse 0.16.0");
        let limit = std::time::Duration::from_secs(5);
        let (bad, good) = (reported_version_within(&bad, limit), reported_version_within(&good, limit));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((bad, good), (None, Some("0.16.0".to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn a_host_uninstalled_while_it_probed_does_not_serve() {
        let dir = scratch("uninstalled-meanwhile");
        let (own, other) = (dir.join("own/fastbrowse"), dir.join("other/fastbrowse"));
        script(&own, "echo fastbrowse 0.13.0");
        script(&other, "echo fastbrowse 0.16.0");
        write_host(&dir, &[at(&own, "0.13.0"), at(&other, "0.16.0")]).unwrap();
        let uninstall_meanwhile = |_: &Path| {
            std::fs::remove_file(dir.join(INSTALLS)).unwrap();
            Some("0.16.0".to_string())
        };
        let planned = plan(&dir, &own, "0.13.0", false, &uninstall_meanwhile);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(planned, Ok(Some((false, vec![]))));
    }

    #[cfg(unix)]
    #[test]
    fn a_host_unregistered_while_it_probed_hands_over_to_the_older_install() {
        let dir = scratch("unregistered-meanwhile");
        let (own, older) = (dir.join("own/fastbrowse"), dir.join("older/fastbrowse"));
        script(&own, "echo fastbrowse 0.16.0");
        script(&older, "echo fastbrowse 0.15.0");
        write_host(&dir, &[at(&own, "0.16.0"), at(&older, "0.15.0")]).unwrap();
        let uninstall_own_meanwhile = |_: &Path| {
            write_host(&dir, &[at(&older, "0.15.0")]).unwrap();
            Some("0.15.0".to_string())
        };
        let planned = plan(&dir, &own, "0.16.0", false, &uninstall_own_meanwhile);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(planned, Ok(Some((false, vec![at(&older, "0.15.0")]))));
    }

    #[cfg(unix)]
    #[test]
    fn the_last_uninstall_fails_when_the_registry_cannot_be_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("undeletable");
        let program = dir.join("bin/fastbrowse");
        script(&program, "echo fastbrowse 1.0.0");
        write_host(&dir, &[at(&program, "1.0.0")]).unwrap();
        locked(&dir, || Ok(())).unwrap();
        // The registry stays readable; a read-only folder only stops it being deleted.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::File::create(dir.join("probe")).is_ok();
        let result = (!writable).then(|| unregister(&dir, &canonical(&program), &|_: &Path| None, Vec::new));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // Running as root, a read-only folder does not stop the delete: nothing to assert.
        if let Some(result) = result {
            assert!(result.unwrap_err().contains("cannot remove"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_path_lookup_skips_a_non_executable_fastbrowse() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("path-lookup");
        let (plain, runnable) = (dir.join("a/fastbrowse"), dir.join("b/fastbrowse"));
        script(&plain, "echo a");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        script(&runnable, "echo b");
        let path = std::env::join_paths([plain.parent().unwrap(), runnable.parent().unwrap()]).unwrap();
        let found = std::env::split_paths(&path).map(|d| d.join("fastbrowse")).find(|c| is_runnable(c));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(runnable));
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_version_probe_is_killed_and_a_delegated_host_probes_nothing() {
        let dir = scratch("hung");
        let (hung, own, probed) = (dir.join("hung/fastbrowse"), dir.join("own/fastbrowse"), dir.join("probed"));
        script(&hung, &format!("touch {}\nexec sleep 30", probed.display()));
        script(&own, "echo fastbrowse 0.13.0");
        let started = std::time::Instant::now();
        let version = reported_version_within(&hung, std::time::Duration::from_millis(300));
        let waited = started.elapsed();
        assert!(probed.exists());
        std::fs::remove_file(&probed).unwrap();
        let installs = vec![at(&own, "0.13.0"), at(&hung, "0.16.0")];
        write_host(&dir, &installs).unwrap();
        let Ok(Some((registered, candidates))) = plan(&dir, &own, "0.13.0", true, &reported_version) else {
            panic!("no plan")
        };
        let probed_by_delegate = probed.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(version, None);
        assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
        assert!(registered && candidates.is_empty());
        assert!(!probed_by_delegate, "a delegated host ran a version probe");
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_whose_helper_holds_stdout_open_is_bounded_by_the_deadline() {
        let dir = scratch("helper");
        let wrapper = dir.join("fastbrowse");
        script(&wrapper, "echo fastbrowse 0.14.0\nsleep 30 &\nexit 0");
        let started = std::time::Instant::now();
        let version = reported_version_within(&wrapper, std::time::Duration::from_millis(500));
        let waited = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(version, None);
        assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_newer_install_does_not_shadow_a_working_one() {
        let dir = scratch("broken");
        let (own, broken, working) =
            (dir.join("own/fastbrowse"), dir.join("broken/fastbrowse"), dir.join("working/fastbrowse"));
        script(&own, "echo fastbrowse 0.13.0");
        script(&working, "echo fastbrowse 0.15.0");
        // Registered as 0.16 but not executable: it cannot report a version, so it cannot be started.
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "not a program").unwrap();
        let installs = vec![at(&own, "0.13.0"), at(&broken, "0.16.0"), at(&working, "0.15.0")];
        let candidates = newer_installs(&installs, &own, "0.13.0", &reported_version);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(candidates, vec![at(&working, "0.15.0")]);
    }

    #[cfg(unix)]
    #[test]
    fn the_handover_runs_the_native_host_with_chromes_origin() {
        let dir = scratch("handover");
        let newer = dir.join("newer/fastbrowse");
        script(&newer, "echo \"$@\" \"$FASTBROWSE_HOST_DELEGATED\"");
        let out = handover_command(&at(&newer, "0.15.0"), &["chrome-extension://x/".into()]).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "native-host chrome-extension://x/ 1\n");
    }

    #[cfg(unix)]
    #[test]
    fn the_host_hands_over_to_an_install_upgraded_in_place() {
        let dir = scratch("upgraded");
        let bundled = dir.join("bundled/fastbrowse");
        let user = dir.join("user/fastbrowse");
        script(&bundled, "echo fastbrowse 0.13.5");
        script(&user, "echo fastbrowse 0.14.0 \\(abc123\\)");
        // The user's copy registered as 0.13.0 and was upgraded in place since; the registry still says 0.13.0.
        let installs = vec![at(&bundled, "0.13.5"), at(&user, "0.13.0")];
        let from_bundled = newer_installs(&installs, &bundled, "0.13.5", &reported_version);
        let from_user = newer_installs(&installs, &user, "0.14.0", &reported_version);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(from_bundled, vec![at(&user, "0.14.0")]);
        assert_eq!(from_user, vec![]);
    }

    #[cfg(unix)]
    #[test]
    fn install_and_uninstall_keep_the_host_while_another_install_is_registered() {
        let dir = scratch("flow");
        let bundled = dir.join("bundled/fastbrowse");
        let user = dir.join("user/fastbrowse");
        let stranger = dir.join("stranger/fastbrowse");
        for (program, version) in [(&bundled, "0.13.0"), (&user, "0.13.0"), (&stranger, "0.13.0")] {
            script(program, &format!("echo fastbrowse {version}"));
        }
        let query = |path: &Path| reported_version(path);
        let register = |_: &Path, _: &str| Ok(vec!["Chrome"]);
        let (bundled, user, stranger) = (canonical(&bundled), canonical(&user), canonical(&stranger));
        let mut out = String::new();
        record_install(&dir, at(&bundled, "0.13.0"), &query, &mut out, register).unwrap();
        let mut second = String::new();
        record_install(&dir, at(&user, "0.13.0"), &query, &mut second, register).unwrap();
        // The same release twice: the earlier registration stays first, and it is not called newer.
        assert!(second.contains("is the same version"), "{second}");
        assert!(!second.contains("is newer"), "{second}");

        let not_registered = unregister(&dir, &stranger, &query, Vec::new).unwrap();
        assert_eq!(not_registered, Uninstalled::NotRegistered(vec![at(&bundled, "0.13.0"), at(&user, "0.13.0")]));
        assert!(dir.join("native-host").exists());
        // A retry repairs a launcher left stale: the registry already lists only these, so the launcher must too.
        write_launcher(&dir, std::slice::from_ref(&stranger)).unwrap();
        unregister(&dir, &stranger, &query, Vec::new).unwrap();
        let repaired = std::fs::read_to_string(dir.join("native-host")).unwrap();
        assert!(
            repaired.contains(bundled.to_str().unwrap()) && !repaired.contains(stranger.to_str().unwrap()),
            "{repaired}"
        );

        let kept = unregister(&dir, &bundled, &query, || panic!("the host must stay")).unwrap();
        assert_eq!(kept, Uninstalled::Kept(vec![at(&user, "0.13.0")]));
        assert_eq!(read_installs(&dir).unwrap(), vec![at(&user, "0.13.0")]);
        assert!(dir.join("native-host").exists());

        let last = unregister(&dir, &user, &query, || vec!["Chrome"]).unwrap();
        let left = [
            dir.join("native-host").exists(),
            dir.join(INSTALLS).exists(),
            dir.join(format!("{HOST_NAME}.json")).exists(),
        ];
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(last, Uninstalled::Removed(vec!["Chrome"]));
        assert_eq!(left, [false; 3]);
    }

    /// An app's bundled fastbrowse and the user's own each register; the newest serves, and the older one takes over
    /// when the newer is removed, where the second install used to replace the first.
    #[cfg(unix)]
    #[test]
    fn every_install_stays_registered_and_the_newest_serves() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("fastbrowse-installs-{}", std::process::id()));
        let mut programs = vec![];
        for name in ["bundled", "user"] {
            let bin = root.join(name);
            std::fs::create_dir_all(&bin).unwrap();
            let program = bin.join("fastbrowse");
            std::fs::write(&program, format!("#!/bin/sh\necho {name} \"$@\"\n")).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            programs.push(program);
        }
        let install = |path: &Path, version: &str| Install { path: path.to_path_buf(), version: version.into() };
        let installs = with_install(vec![], install(&programs[1], "0.13.0"));
        // The app installs later with an older bundled copy; the user's newer one keeps serving.
        let installs = with_install(installs, install(&programs[0], "0.12.0"));
        assert_eq!(installs, [install(&programs[1], "0.13.0"), install(&programs[0], "0.12.0")]);
        // Installing a version again replaces its own entry instead of adding a second.
        let installs = with_install(installs, install(&programs[0], "0.14.0"));
        assert_eq!(installs, [install(&programs[0], "0.14.0"), install(&programs[1], "0.13.0")]);

        let launcher = write_host(&root, &installs).unwrap();
        assert_eq!(read_installs(&root).unwrap(), installs);
        let run = || {
            let out = std::process::Command::new(&launcher).arg("chrome-extension://x/").output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let newest = run();
        std::fs::remove_dir_all(root.join("bundled")).unwrap();
        let fallback = run();
        // An install whose program is gone drops out the next time another registers.
        let pruned = with_install(installs, install(&programs[1], "0.13.0"));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(newest, "bundled native-host chrome-extension://x/\n");
        assert_eq!(fallback, "user native-host chrome-extension://x/\n");
        assert_eq!(pruned, [install(&programs[1], "0.13.0")]);
    }
}
