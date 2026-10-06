"""`scripts/npm_platform_package.py`: one frozen build in, one publishable `@fastbrowse/<os>-<arch>` package out."""

import json
import os
import re
import subprocess
import sys
import tarfile
from pathlib import Path
from typing import Any

import pytest

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "npm_platform_package.py"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")

needs_modes = pytest.mark.skipif(os.name == "nt", reason="Windows keeps no executable bit")


def _frozen(root: Path, *, version: str = "1.2.3", executable: str = "fastbrowse") -> Path:
    """A directory shaped like PyInstaller's output: the executable, and `_internal/` beside it."""
    tree = root / "fastbrowse"
    internal = tree / "_internal"
    (internal / f"fastbrowse-{version}.dist-info").mkdir(parents=True)
    (internal / f"fastbrowse-{version}.dist-info" / "METADATA").write_text(f"Name: fastbrowse\nVersion: {version}\n")
    (internal / "fastbrowse" / "browser").mkdir(parents=True)
    (internal / "fastbrowse" / "browser" / "snapshot.js").write_text("// the page script\n")
    (internal / "libpython3.13.so.1.0").write_bytes(b"\x7fELF")
    (tree / executable).write_bytes(b"\x7fELF the bootloader")
    (tree / executable).chmod(0o755)
    return tree


def _assemble(frozen: Path, target: str, version: str, out: Path) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(SCRIPT), str(frozen), "--target", target, "--version", version, "--out", str(out)]
    return subprocess.run(command, capture_output=True, text=True)


def _assembled(frozen: Path, target: str, version: str, out: Path) -> dict[str, Any]:
    done = _assemble(frozen, target, version, out)
    assert done.returncode == 0, done.stderr
    return json.loads((out / "package.json").read_text())


def test_the_package_is_named_for_its_target_and_installs_only_on_it(tmp_path: Path) -> None:
    manifest = _assembled(_frozen(tmp_path), "darwin-arm64", "1.2.3", tmp_path / "package")

    assert manifest["name"] == "@fastbrowse/darwin-arm64"
    assert manifest["version"] == "1.2.3"
    assert manifest["os"] == ["darwin"]
    assert manifest["cpu"] == ["arm64"]


@pytest.mark.parametrize(
    ("target", "system", "cpu", "libc"),
    [
        ("darwin-x64", "darwin", "x64", None),
        # The Linux builds link glibc, and npm leaves a package that says so out of an install on musl.
        ("linux-arm64", "linux", "arm64", ["glibc"]),
        ("linux-x64", "linux", "x64", ["glibc"]),
    ],
)
def test_each_target_declares_its_own_system(
    tmp_path: Path, target: str, system: str, cpu: str, libc: list[str] | None
) -> None:
    manifest = _assembled(_frozen(tmp_path), target, "1.2.3", tmp_path / "package")

    assert (manifest["os"], manifest["cpu"], manifest.get("libc")) == ([system], [cpu], libc)


def test_nothing_runs_when_the_package_is_installed(tmp_path: Path) -> None:
    manifest = _assembled(_frozen(tmp_path), "linux-x64", "1.2.3", tmp_path / "package")

    assert "scripts" not in manifest
    assert "bin" not in manifest
    assert "dependencies" not in manifest


def test_the_package_can_be_published_from_this_repository(tmp_path: Path) -> None:
    manifest = _assembled(_frozen(tmp_path), "linux-x64", "1.2.3", tmp_path / "package")

    # A scoped package is private unless told otherwise, and provenance is checked against the repository.
    assert manifest["publishConfig"] == {"access": "public"}
    assert manifest["repository"]["url"] == "git+https://github.com/agent-labs-dev/fastbrowse.git"
    assert manifest["license"] == "MIT"
    assert (tmp_path / "package" / "LICENSE").read_text() == (REPO / "LICENSE").read_text()
    assert (tmp_path / "package" / "NOTICE").read_text() == (REPO / "NOTICE").read_text()
    assert "@fastbrowse/linux-x64" in (tmp_path / "package" / "README.md").read_text()


def test_the_build_is_carried_whole_under_fastbrowse(tmp_path: Path) -> None:
    frozen = _frozen(tmp_path)
    out = tmp_path / "package"
    manifest = _assembled(frozen, "linux-x64", "1.2.3", out)

    carried = sorted(path.relative_to(out / "fastbrowse") for path in (out / "fastbrowse").rglob("*"))
    assert carried == sorted(path.relative_to(frozen) for path in frozen.rglob("*"))
    assert (out / "fastbrowse" / "fastbrowse").read_bytes() == (frozen / "fastbrowse").read_bytes()
    assert "fastbrowse" in manifest["files"]


@needs_modes
def test_the_executable_keeps_its_executable_bit(tmp_path: Path) -> None:
    frozen = _frozen(tmp_path)
    # What downloading an artifact that was not archived does to it.
    (frozen / "fastbrowse").chmod(0o644)
    out = tmp_path / "package"
    _assembled(frozen, "linux-x64", "1.2.3", out)

    assert (out / "fastbrowse" / "fastbrowse").stat().st_mode & 0o111 == 0o111
    assert (out / "fastbrowse" / "_internal" / "fastbrowse" / "browser" / "snapshot.js").stat().st_mode & 0o111 == 0


@needs_modes
def test_the_archive_the_binaries_workflow_uploads_is_taken_as_it_is(tmp_path: Path) -> None:
    frozen = _frozen(tmp_path / "dist")
    archive = tmp_path / "fastbrowse-linux-x64.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(frozen, arcname="fastbrowse")
    out = tmp_path / "package"
    _assembled(archive, "linux-x64", "1.2.3", out)

    assert (out / "fastbrowse" / "fastbrowse").stat().st_mode & 0o111 == 0o111
    assert (out / "fastbrowse" / "_internal" / "fastbrowse" / "browser" / "snapshot.js").is_file()


@needs_modes
def test_a_symbolic_link_is_carried_as_the_file_it_points_to(tmp_path: Path) -> None:
    frozen = _frozen(tmp_path)
    # npm leaves links out of a package, so one left as a link would be a library missing after the install.
    (frozen / "_internal" / "libpython3.13.so").symlink_to("libpython3.13.so.1.0")
    out = tmp_path / "package"
    _assembled(frozen, "linux-x64", "1.2.3", out)

    copied = out / "fastbrowse" / "_internal" / "libpython3.13.so"
    assert not copied.is_symlink()
    assert copied.read_bytes() == b"\x7fELF"


def test_what_macos_tar_adds_to_an_archive_is_left_out(tmp_path: Path) -> None:
    frozen = _frozen(tmp_path / "dist")
    archive = tmp_path / "fastbrowse-darwin-arm64.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(frozen, arcname="fastbrowse")
        # bsdtar writes a file's extended attributes as a second entry, named for the file with `._` in front.
        tar.add(frozen / "_internal" / "libpython3.13.so.1.0", arcname="fastbrowse/._fastbrowse")
        tar.add(frozen / "_internal" / "libpython3.13.so.1.0", arcname="fastbrowse/_internal/._libpython3.13.so.1.0")
    out = tmp_path / "package"
    _assembled(archive, "darwin-arm64", "1.2.3", out)

    carried = sorted(path.relative_to(out / "fastbrowse") for path in (out / "fastbrowse").rglob("*"))
    assert carried == sorted(path.relative_to(frozen) for path in frozen.rglob("*"))


def test_a_windows_package_holds_the_exe(tmp_path: Path) -> None:
    out = tmp_path / "package"
    manifest = _assembled(_frozen(tmp_path, executable="fastbrowse.exe"), "win32-x64", "1.2.3", out)

    assert (manifest["os"], manifest["cpu"]) == (["win32"], ["x64"])
    assert (out / "fastbrowse" / "fastbrowse.exe").is_file()


def test_a_build_for_another_system_is_refused(tmp_path: Path) -> None:
    done = _assemble(_frozen(tmp_path), "win32-x64", "1.2.3", tmp_path / "package")

    assert done.returncode == 1
    assert "fastbrowse.exe" in done.stderr
    assert not (tmp_path / "package").exists()


def test_a_version_the_build_does_not_hold_is_refused(tmp_path: Path) -> None:
    done = _assemble(_frozen(tmp_path, version="1.2.3"), "linux-x64", "1.2.4", tmp_path / "package")

    assert done.returncode == 1
    assert "1.2.3" in done.stderr
    assert "1.2.4" in done.stderr
    assert not (tmp_path / "package").exists()


def test_a_directory_with_something_in_it_is_not_written_over(tmp_path: Path) -> None:
    out = tmp_path / "package"
    out.mkdir()
    (out / "kept").write_text("someone's file")
    done = _assemble(_frozen(tmp_path), "linux-x64", "1.2.3", out)

    assert done.returncode == 1
    assert sorted(path.name for path in out.iterdir()) == ["kept"]


def test_the_targets_are_the_ones_built_and_the_ones_the_sdk_depends_on() -> None:
    built = re.findall(r"\{target: ([\w-]+), runner:", (REPO / ".github/workflows/binaries.yml").read_text())
    sdk = json.loads((REPO / "packages/sdk/package.json").read_text())

    assert sorted(built) == sorted(TARGETS)
    assert sorted(sdk["optionalDependencies"]) == sorted(f"@fastbrowse/{target}" for target in TARGETS)
    for target in TARGETS:
        assert _assemble(Path("no-such-build"), target, "1.2.3", Path("unused")).stderr.startswith("refused: ")
