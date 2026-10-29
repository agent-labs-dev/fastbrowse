"""`scripts/go_binaries.py`: a release pins each platform's archive in the Go module by its SHA-256."""

import hashlib
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "go_binaries.py"
MODULE = Path(__file__).resolve().parent.parent / "go" / "binaries.go"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")


def _archives(root: Path, targets: tuple[str, ...] = TARGETS) -> list[Path]:
    paths = []
    for target in targets:
        path = root / f"fastbrowse-rust-{target}" / f"fastbrowse-rust-{target}.tar.gz"
        path.parent.mkdir()
        path.write_bytes(target.encode())
        paths.append(path)
    return paths


def _run(file: Path, *arguments: object) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(SCRIPT), "--file", str(file), *map(str, arguments)]
    return subprocess.run(command, capture_output=True, text=True)


def test_the_module_file_gets_the_version_and_each_archives_checksum(tmp_path: Path) -> None:
    file = tmp_path / "binaries.go"
    file.write_text(MODULE.read_text())

    done = _run(file, "1.2.3", *_archives(tmp_path))

    assert done.returncode == 0, done.stderr
    written = file.read_text()
    assert 'var releaseVersion = "1.2.3"' in written
    assert f'"linux-x64":    "{hashlib.sha256(b"linux-x64").hexdigest()}"' in written
    # The comment above it says what the values are, and stays.
    assert written.startswith(MODULE.read_text().split("package")[0])


def test_a_release_missing_a_platform_pins_nothing(tmp_path: Path) -> None:
    file = tmp_path / "binaries.go"
    file.write_text(MODULE.read_text())

    done = _run(file, "1.2.3", *_archives(tmp_path, TARGETS[:-1]))

    assert done.returncode == 1
    assert "win32-x64" in done.stderr
    assert file.read_text() == MODULE.read_text()


def test_the_checkout_pins_no_release() -> None:
    # A copy of the module from `main` downloads nothing; only the commit a release tags does.
    assert 'var releaseVersion = ""' in MODULE.read_text()
