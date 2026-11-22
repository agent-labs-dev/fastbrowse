//! `fastbrowse native-host` hands over to a newer registered install, found from the launcher's folder.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn native_host_hands_over_with_chromes_origin_from_the_launchers_registry() {
    let root = std::env::temp_dir().join(format!("fastbrowse-handover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let newer = root.join("newer/fastbrowse");
    script(&newer, "if [ \"$1\" = --version ]; then echo fastbrowse 999.0.0; else echo \"$@\"; fi");
    let host_dir = root.join("host");
    std::fs::create_dir_all(&host_dir).unwrap();
    let registry = serde_json::json!([{"path": newer, "version": "999.0.0"}]);
    std::fs::write(host_dir.join("installs.json"), registry.to_string()).unwrap();
    // The environment Chrome starts the host in need not match the one install ran in: only the launcher's folder says.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fastbrowse"))
        .args(["native-host", "chrome-extension://x/"])
        .env("FASTBROWSE_HOST_DIR", &host_dir)
        .env("XDG_DATA_HOME", root.join("elsewhere"))
        .env_remove("FASTBROWSE_HOST_DELEGATED")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "native-host chrome-extension://x/\n");
}

#[test]
fn an_unregistered_native_host_exits_naming_the_registry_when_no_install_starts() {
    let root = std::env::temp_dir().join(format!("fastbrowse-unregistered-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let removed = root.join("removed/fastbrowse");
    let host_dir = root.join("host");
    std::fs::create_dir_all(&host_dir).unwrap();
    let registry = serde_json::json!([{"path": removed, "version": "999.0.0"}]);
    std::fs::write(host_dir.join("installs.json"), registry.to_string()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fastbrowse"))
        .args(["native-host", "chrome-extension://x/"])
        .env("FASTBROWSE_HOST_DIR", &host_dir)
        .env_remove("FASTBROWSE_HOST_DELEGATED")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("installs.json"), "{}", String::from_utf8_lossy(&out.stderr));
}

fn host(host_dir: &Path, started: Option<&Path>) -> std::process::Output {
    host_as(host_dir, started, false)
}

fn host_as(host_dir: &Path, started: Option<&Path>, delegated: bool) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_fastbrowse"));
    command
        .args(["native-host", "chrome-extension://ghodfgfhbfaghhfdabopagbfopdjgpop/"])
        .env("FASTBROWSE_HOST_DIR", host_dir)
        .env_remove("FASTBROWSE_HOST_PATH")
        .stdin(std::process::Stdio::null());
    if delegated {
        command.env("FASTBROWSE_HOST_DELEGATED", "1");
    } else {
        command.env_remove("FASTBROWSE_HOST_DELEGATED");
    }
    if let Some(started) = started {
        command.env("FASTBROWSE_HOST_PATH", started);
    }
    command.output().unwrap()
}

#[test]
fn the_host_identity_is_the_path_the_launcher_started_not_this_executable() {
    let root = std::env::temp_dir().join(format!("fastbrowse-identity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let host_dir = root.join("host");
    std::fs::create_dir_all(&host_dir).unwrap();
    // A Python install: the registered script is not the executable that runs it.
    let started = root.join("tool/bin/fastbrowse");
    let registry = serde_json::json!([{"path": started, "version": "0.0.1"}]);
    std::fs::write(host_dir.join("installs.json"), registry.to_string()).unwrap();
    let registered = host(&host_dir, Some(&started));
    let unregistered = host(&host_dir, Some(&root.join("stray/fastbrowse")));
    // No registry at all: a launcher from before installs were recorded.
    std::fs::remove_file(host_dir.join("installs.json")).unwrap();
    let before_registry = host(&host_dir, None);
    let after_uninstall = host(&host_dir, Some(&started));
    // The registry's folder denies traversal, so its metadata cannot be read.
    std::fs::write(host_dir.join("installs.json"), "[]").unwrap();
    std::fs::set_permissions(&host_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Run as the legacy launcher (no path named): only the metadata check tells this from a missing registry.
    let unreadable = host(&host_dir, None);
    std::fs::set_permissions(&host_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(host_dir.join("installs.json")).unwrap();
    // A registry that cannot be parsed is an error that names it.
    std::fs::write(host_dir.join("installs.json"), "{").unwrap();
    let corrupt = host(&host_dir, Some(&started));
    // A delegated host must itself be registered: here both installs were unregistered after the handover began.
    std::fs::write(host_dir.join("installs.json"), "[]").unwrap();
    let delegated_unregistered = host_as(&host_dir, Some(&started), true);
    std::fs::write(host_dir.join("installs.json"), registry.to_string()).unwrap();
    let delegated_registered = host_as(&host_dir, Some(&started), true);
    let _ = std::fs::remove_dir_all(&root);
    // Serving: the host got past the handover decision and waits for the extension's hello, which a closed stdin denies.
    let serving = |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).contains("before saying hello");
    let said = |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(serving(&registered), "{}", said(&registered));
    assert!(said(&unregistered).contains("not registered"), "{}", said(&unregistered));
    assert!(serving(&before_registry), "{}", said(&before_registry));
    // A registry-aware launcher (it names the path) that finds no registry was started just before the last uninstall.
    assert_eq!(after_uninstall.status.code(), Some(1));
    assert!(said(&after_uninstall).contains("installs.json"), "{}", said(&after_uninstall));
    assert!(!serving(&after_uninstall));
    // A registry that cannot be inspected is not a missing one.
    assert_eq!(unreadable.status.code(), Some(1));
    assert!(said(&unreadable).contains("installs.json"), "{}", said(&unreadable));
    assert!(!serving(&unreadable));
    assert_eq!(delegated_unregistered.status.code(), Some(1));
    assert!(said(&delegated_unregistered).contains("not registered"), "{}", said(&delegated_unregistered));
    assert!(serving(&delegated_registered), "{}", said(&delegated_registered));
    assert_eq!(corrupt.status.code(), Some(1));
    assert!(said(&corrupt).contains("installs.json"), "{}", said(&corrupt));
}
