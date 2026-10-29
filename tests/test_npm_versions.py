"""`scripts/npm_versions.py`: the npm packages and the Rust workspace carry the Python package's version, or the
check fails."""

import json
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "npm_versions.py"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")


LOCK = """version = 4

[[package]]
name = "fastbrowse-core"
version = "{rust}"
dependencies = [
 "url",
]

[[package]]
name = "fastbrowse-probe"
version = "0.0.0"

[[package]]
name = "url"
version = "{rust}"
source = "registry+https://github.com/rust-lang/crates.io-index"
"""


def _checkout(
    root: Path,
    *,
    python: str = "1.2.3",
    sdk: str = "1.2.3",
    rust: str = "1.2.3",
    crate_pin: str | None = None,
    pins: dict[str, str] | None = None,
) -> Path:
    """A directory holding the files the check reads, as a checkout does."""
    (root / "pyproject.toml").write_text(f'[project]\nname = "fastbrowse"\nversion = "{python}"\n')
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = ["crates/fastbrowse-core"]\n\n'
        f'[workspace.package]\nedition = "2024"\nversion = "{rust}"\n\n'
        f"[workspace.dependencies]\n"
        f'fastbrowse-core = {{ path = "crates/fastbrowse-core", version = "={crate_pin or rust}" }}\n'
        f'serde = {{ version = "1" }}\n'
    )
    (root / "Cargo.lock").write_text(LOCK.format(rust=rust))
    manifest = {
        "name": "fastbrowse",
        "version": sdk,
        "license": "MIT",
        "optionalDependencies": {f"@fastbrowse/{target}": "1.2.3" for target in TARGETS} | (pins or {}),
    }
    (root / "packages" / "sdk").mkdir(parents=True)
    (root / "packages" / "sdk" / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return root


def _run(root: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(SCRIPT), "--root", str(root), *arguments]
    return subprocess.run(command, capture_output=True, text=True)


def _manifest(root: Path) -> dict[str, Any]:
    return json.loads((root / "packages" / "sdk" / "package.json").read_text())


def test_one_version_everywhere_passes(tmp_path: Path) -> None:
    done = _run(_checkout(tmp_path))

    assert done.returncode == 0, done.stderr
    assert "1.2.3" in done.stdout


def test_an_sdk_at_another_version_fails_and_is_named(tmp_path: Path) -> None:
    done = _run(_checkout(tmp_path, python="1.2.4"))

    assert done.returncode == 1
    assert "fastbrowse is 1.2.3" in done.stderr
    assert "1.2.4" in done.stderr


def test_a_platform_package_pinned_to_another_version_fails_and_is_named(tmp_path: Path) -> None:
    done = _run(_checkout(tmp_path, pins={"@fastbrowse/linux-arm64": "1.2.2"}))

    assert done.returncode == 1
    assert "@fastbrowse/linux-arm64 is 1.2.2" in done.stderr
    assert "@fastbrowse/linux-x64" not in done.stderr


def test_a_range_is_not_the_version(tmp_path: Path) -> None:
    # `^1.2.3` would let an SDK start a later binary than the one it was released with.
    done = _run(_checkout(tmp_path, pins={"@fastbrowse/win32-x64": "^1.2.3"}))

    assert done.returncode == 1
    assert "@fastbrowse/win32-x64 is ^1.2.3" in done.stderr


def test_a_platform_package_that_is_missing_or_unknown_fails(tmp_path: Path) -> None:
    root = _checkout(tmp_path, pins={"@fastbrowse/linux-riscv64": "1.2.3"})
    manifest = _manifest(root)
    del manifest["optionalDependencies"]["@fastbrowse/darwin-x64"]
    (root / "packages" / "sdk" / "package.json").write_text(json.dumps(manifest))

    done = _run(root)

    assert done.returncode == 1
    assert "@fastbrowse/darwin-x64 is missing" in done.stderr
    assert "@fastbrowse/linux-riscv64 is not a platform package" in done.stderr


def test_write_moves_every_npm_version_to_the_python_one_and_keeps_the_rest(tmp_path: Path) -> None:
    root = _checkout(tmp_path, python="2.0.0")

    assert _run(root, "--write").returncode == 0

    manifest = _manifest(root)
    assert manifest["version"] == "2.0.0"
    assert manifest["optionalDependencies"] == {f"@fastbrowse/{target}": "2.0.0" for target in TARGETS}
    assert manifest["license"] == "MIT"
    assert list(manifest) == ["name", "version", "license", "optionalDependencies"]
    assert _run(root).returncode == 0


def test_a_rust_workspace_at_another_version_fails_and_is_named(tmp_path: Path) -> None:
    # The Rust binary prints this for `--version` and hands it to a `serve` client, so it must be the release's.
    done = _run(_checkout(tmp_path, rust="1.2.2"))

    assert done.returncode == 1
    assert "the workspace is 1.2.2" in done.stderr


def test_a_crate_pinned_to_another_version_fails_and_is_named(tmp_path: Path) -> None:
    # crates.io would take a crate depending on another release of its siblings than its own.
    done = _run(_checkout(tmp_path, crate_pin="1.2.2"))

    assert done.returncode == 1
    assert "fastbrowse-core is pinned to =1.2.2" in done.stderr


def test_a_crate_pin_without_the_equals_sign_is_a_range_and_fails(tmp_path: Path) -> None:
    # Cargo reads a bare `1.2.3` as `^1.2.3`, which lets a crate resolve a later sibling than its own release.
    root = _checkout(tmp_path)
    cargo = root / "Cargo.toml"
    cargo.write_text(cargo.read_text().replace('version = "=1.2.3"', 'version = "1.2.3"'))

    done = _run(root)

    assert done.returncode == 1
    assert "fastbrowse-core is pinned to 1.2.3" in done.stderr


def test_write_moves_the_rust_workspace_and_its_own_lock_entries_only(tmp_path: Path) -> None:
    root = _checkout(tmp_path, python="2.0.0")

    assert _run(root, "--write").returncode == 0

    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    assert cargo["workspace"]["package"] == {"edition": "2024", "version": "2.0.0"}
    # The crates published together pin each other to the release; a registry crate keeps its own requirement.
    assert cargo["workspace"]["dependencies"] == {
        "fastbrowse-core": {"path": "crates/fastbrowse-core", "version": "=2.0.0"},
        "serde": {"version": "1"},
    }
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    # A registry crate that happens to share the old number keeps it, and so does a member with its own version.
    assert {package["name"]: package["version"] for package in lock["package"]} == {
        "fastbrowse-core": "2.0.0",
        "fastbrowse-probe": "0.0.0",
        "url": "1.2.3",
    }
    assert _run(root).returncode == 0


def test_this_checkout_is_in_lockstep() -> None:
    done = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)

    assert done.returncode == 0, done.stderr
