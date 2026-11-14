//! What a new user actually gets from the Chrome Web Store, against what this tree's runs need.
//!
//! The extension and the CLI reach users on two channels with different lag: the Store reviews an upload, PyPI takes a
//! tag. Nothing in a single commit can see both, so a released CLI can require methods the approved package does not
//! relay and every Store install then refuses every run (#1063). These tests hold the two facts that make that
//! visible: the package the Store serves today, and the gap between it and [`required_methods`].

use std::path::{Path, PathBuf};

use fastbrowse_browser::extension::protocol::{MIN_EXTENSION_VERSION, allowed_methods, required_methods};
use serde_json::Value;

fn extension_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("extension")
}

fn store_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("extension-store")
}

fn compat() -> Value {
    let text = std::fs::read_to_string(store_dir().join("compat.json")).expect("extension-store/compat.json");
    serde_json::from_str(&text).expect("compat.json is JSON")
}

/// The files of the package the Store serves today, as `extension-store/compat.json` names them.
fn published() -> (String, Vec<String>, Value) {
    let record = compat();
    let dir = store_dir().join(record["store"]["snapshot"].as_str().expect("store.snapshot"));
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let methods: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("allowed-methods.json")).unwrap()).unwrap();
    (manifest["version"].as_str().expect("manifest version").to_owned(), methods, record)
}

fn not_sent(published: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = allowed_methods().into_iter().filter(|m| !published.contains(m)).collect();
    missing.sort();
    missing
}

fn not_relayed(published: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = required_methods().into_iter().filter(|m| !published.contains(m)).collect();
    missing.sort();
    missing
}

/// The copy this tree ships is the one `fastbrowse extension path` writes, so it must satisfy this build by
/// construction: a method a run needs that it does not relay is a bug here, not a Store problem.
#[test]
fn the_extension_this_build_ships_relays_every_method_a_run_requires() {
    let ours = allowed_methods();
    let missing: Vec<_> = required_methods().into_iter().filter(|method| !ours.contains(method)).collect();
    assert!(missing.is_empty(), "add {missing:?} to extension/allowed-methods.json or stop requiring them");
}

/// The gap recorded for the Store package is the gap the snapshot actually has. A method added to the required set
/// changes the computed gap without anyone touching `compat.json`, so this fails until the release is made to say
/// what it is waiting for.
#[test]
fn the_gap_recorded_for_the_store_package_is_the_one_the_snapshot_has() {
    let (version, methods, record) = published();
    assert_eq!(
        version,
        record["store"]["version"].as_str().unwrap(),
        "the snapshot is not the package compat.json names"
    );
    let mut recorded: Vec<String> =
        serde_json::from_value(record["store"]["missing"].clone()).expect("store.missing is a list of methods");
    recorded.sort();
    assert_eq!(not_sent(&methods), recorded, "store.missing no longer describes what the package lacks");
    let blocking: Vec<String> =
        serde_json::from_value(record["store"]["blocking"].clone()).expect("store.blocking is a list of methods");
    assert_eq!(not_relayed(&methods), blocking, "store.blocking no longer describes what a run cannot do without");
}

/// A build the Store package cannot satisfy says so, names the version it needs, and never claims a compatibility the
/// snapshot disproves; one it can satisfy says that instead. Flipping `needs` to a version the Store does not serve is
/// what makes a release wait for the upload instead of breaking every new install.
#[test]
fn the_store_package_this_tree_cannot_be_released_against_says_so() {
    let (_, methods, record) = published();
    let missing = not_relayed(&methods);
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(extension_dir().join("manifest.json")).unwrap()).unwrap();
    let shipped = manifest["version"].as_str().expect("manifest version");
    if missing.is_empty() {
        assert_eq!(record["store"]["needs"], shipped, "the Store package satisfies this build: claim that version");
    } else {
        assert_eq!(
            record["store"]["needs"].as_str(),
            Some(MIN_EXTENSION_VERSION),
            "a build the Store cannot satisfy must name the extension version that can, and it is the one this \
             build's runs require"
        );
        assert_eq!(record["store"]["needs"].as_str(), Some(shipped), "the version to wait for is this tree's own");
        assert!(!missing.is_empty());
    }
}

/// The version a run tells a user to install cannot be older than the one this tree ships: a run naming an older
/// extension would send someone to a package that cannot drive it.
#[test]
fn the_version_a_run_asks_for_is_the_one_this_tree_ships_or_newer() {
    let shipped: Value =
        serde_json::from_str(&std::fs::read_to_string(extension_dir().join("manifest.json")).unwrap()).unwrap();
    let shipped: Vec<u64> = shipped["version"].as_str().unwrap().split('.').map(|n| n.parse().unwrap()).collect();
    let needed: Vec<u64> = MIN_EXTENSION_VERSION.split('.').map(|n| n.parse().unwrap()).collect();
    assert!(needed >= shipped, "{MIN_EXTENSION_VERSION} is older than the extension this tree ships");
}
