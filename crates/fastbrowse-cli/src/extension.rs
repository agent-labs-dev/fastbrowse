//! `fastbrowse native-host` and `fastbrowse extension install | uninstall | path`.
//!
//! `install` registers this fastbrowse as the extension's native-messaging host for every Chromium browser it finds:
//! a host manifest per browser, at user level, pointing at a small launcher that runs `<this fastbrowse>
//! native-host`. Chrome starts that launcher when the extension connects. `path` writes the extension's own files
//! out for extension development; ordinary installs use the Chrome Web Store.

use std::path::{Path, PathBuf};

// The extension's files are built into the binary, so `path` works offline and always matches this version.
use fastbrowse_browser::extension::FILES;
use fastbrowse_browser::extension::protocol::{EXTENSION_IDS, HOST_NAME};
use serde_json::json;

const STORE_URL: &str = "https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop";

const USAGE: &str = "usage: fastbrowse extension install [--policy] | uninstall | path";

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

/// The command that runs this fastbrowse again: the program as the user invoked it, found on `PATH` when bare.
fn this_fastbrowse(program: &str) -> PathBuf {
    let named = Path::new(program);
    if named.components().count() > 1 {
        return std::fs::canonicalize(named).unwrap_or_else(|_| named.to_path_buf());
    }
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
    });
    found.or_else(|| std::env::current_exe().ok()).unwrap_or_else(|| named.to_path_buf())
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

fn write_launcher(dir: &Path, fastbrowse: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    if cfg!(windows) {
        let launcher = dir.join("native-host.bat");
        std::fs::write(&launcher, format!("@echo off\r\n\"{}\" native-host %*\r\n", fastbrowse.display()))?;
        return Ok(launcher);
    }
    let launcher = dir.join("native-host");
    let quoted = fastbrowse.display().to_string().replace('\'', r"'\''");
    std::fs::write(&launcher, format!("#!/bin/sh\nexec '{quoted}' native-host \"$@\"\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(launcher)
}

/// Register the host with every browser found; the names of the browsers registered.
fn install(program: &str, out: &mut String) -> Result<Vec<&'static str>, String> {
    let fastbrowse = this_fastbrowse(program);
    if ephemeral(&fastbrowse) {
        out.push_str(&format!(
            "warning: {} looks like a one-off copy from a package runner's cache, which may be deleted. Install \
             fastbrowse (for example `uv tool install fastbrowse`) and run this again from it.\n",
            fastbrowse.display()
        ));
    }
    let dir = data_dir();
    let launcher = write_launcher(&dir, &fastbrowse).map_err(|e| format!("cannot write the launcher: {e}"))?;
    let manifest = host_manifest(&launcher);
    let manifest_path = dir.join(format!("{HOST_NAME}.json"));
    std::fs::write(&manifest_path, &manifest).map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
    register(&manifest_path, &manifest)
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
        std::fs::write(&path, manifest).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
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

/// Remove exactly what `install` wrote.
fn uninstall() -> Vec<&'static str> {
    let mut removed = vec![];
    #[cfg(not(windows))]
    for (name, folder) in BROWSERS {
        let path = config_base().join(folder).join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
        if std::fs::remove_file(path).is_ok() {
            removed.push(name);
        }
    }
    #[cfg(windows)]
    for (name, key) in BROWSERS {
        let key = format!(r"HKCU\{key}\NativeMessagingHosts\{HOST_NAME}");
        let status = std::process::Command::new("reg").args(["delete", &key, "/f"]).status();
        if status.is_ok_and(|s| s.success()) {
            removed.push(name);
        }
    }
    let dir = data_dir();
    for file in ["native-host", "native-host.bat", &format!("{HOST_NAME}.json")] {
        let _ = std::fs::remove_file(dir.join(file));
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
        ["uninstall"] => {
            let removed = uninstall();
            if removed.is_empty() {
                out.push_str("Nothing to remove: the fastbrowse host was not registered.\n");
            } else {
                out.push_str(&format!("Removed the fastbrowse host from {}.\n", removed.join(", ")));
            }
            (out, 0)
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

    #[test]
    fn a_launcher_runs_this_fastbrowse_as_the_host() {
        let dir = std::env::temp_dir().join(format!("fastbrowse-launcher-{}", std::process::id()));
        let launcher = write_launcher(&dir, Path::new("/opt/it's/fastbrowse")).unwrap();
        let text = std::fs::read_to_string(&launcher).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        if cfg!(windows) {
            assert!(text.contains("native-host %*"));
        } else {
            assert_eq!(text, "#!/bin/sh\nexec '/opt/it'\\''s/fastbrowse' native-host \"$@\"\n");
        }
    }
}
