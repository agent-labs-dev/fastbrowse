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
