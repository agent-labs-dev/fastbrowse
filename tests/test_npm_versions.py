"""`scripts/npm_versions.py`: the npm packages carry the Python package's version, or the check fails."""

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "npm_versions.py"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")


def _checkout(root: Path, *, python: str = "1.2.3", sdk: str = "1.2.3", pins: dict[str, str] | None = None) -> Path:
    """A directory holding the two files the check reads, as a checkout does."""
    (root / "pyproject.toml").write_text(f'[project]\nname = "fastbrowse"\nversion = "{python}"\n')
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


def test_this_checkout_is_in_lockstep() -> None:
    done = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)

    assert done.returncode == 0, done.stderr
