"""`scripts/npm_platform_package.py`: one Rust binary in, one publishable `@fastbrowse/<os>-<arch>` package out."""

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


def _rust(root: Path, *, version: str = "1.2.3", name: str = "fastbrowse") -> Path:
    """A file shaped like the Rust executable: the version it reports is a literal among its bytes."""
    root.mkdir(parents=True, exist_ok=True)
    binary = root / name
    binary.write_bytes(b"\x7fELF ... fastbrowse " + version.encode() + b"protocol_version ...")
    binary.chmod(0o755)
    return binary


def _assemble(build: Path, target: str, version: str, out: Path, *extra: str) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(SCRIPT), str(build), "--target", target, "--version", version, "--out", str(out)]
    return subprocess.run([*command, *extra], capture_output=True, text=True)


def _assembled(build: Path, target: str, out: Path, *extra: str) -> dict[str, Any]:
    done = _assemble(build, target, "1.2.3", out, *extra)
    assert done.returncode == 0, done.stderr
    return json.loads((out / "package.json").read_text())


def test_the_package_is_named_for_its_target_and_installs_only_on_it(tmp_path: Path) -> None:
    manifest = _assembled(_rust(tmp_path), "darwin-arm64", tmp_path / "package")

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
    manifest = _assembled(_rust(tmp_path), target, tmp_path / "package")

    assert (manifest["os"], manifest["cpu"], manifest.get("libc")) == ([system], [cpu], libc)


def test_nothing_runs_when_the_package_is_installed(tmp_path: Path) -> None:
    manifest = _assembled(_rust(tmp_path), "linux-x64", tmp_path / "package")

    assert "scripts" not in manifest
    assert "bin" not in manifest
    assert "dependencies" not in manifest


def test_the_package_can_be_published_from_this_repository(tmp_path: Path) -> None:
    manifest = _assembled(_rust(tmp_path), "linux-x64", tmp_path / "package")

    # A scoped package is private unless told otherwise, and provenance is checked against the repository.
    assert manifest["publishConfig"] == {"access": "public"}
    assert manifest["repository"]["url"] == "git+https://github.com/agent-labs-dev/fastbrowse.git"
    assert manifest["license"] == "MIT"
    assert (tmp_path / "package" / "LICENSE").read_text() == (REPO / "LICENSE").read_text()
    assert (tmp_path / "package" / "NOTICE").read_text() == (REPO / "NOTICE").read_text()
    assert "@fastbrowse/linux-x64" in (tmp_path / "package" / "README.md").read_text()


def test_a_windows_package_holds_the_exe(tmp_path: Path) -> None:
    out = tmp_path / "package"
    manifest = _assembled(_rust(tmp_path, name="fastbrowse.exe"), "win32-x64", out)

    assert (manifest["os"], manifest["cpu"]) == (["win32"], ["x64"])
    assert (out / "fastbrowse" / "fastbrowse.exe").is_file()


def test_a_build_for_another_system_is_refused(tmp_path: Path) -> None:
    archive = tmp_path / "fastbrowse-linux-x64.tar.gz"
    with tarfile.open(archive, "w:gz") as packed:
        packed.add(_rust(tmp_path / "build" / "fastbrowse").parent, arcname="fastbrowse")
    done = _assemble(archive, "win32-x64", "1.2.3", tmp_path / "package")

    assert done.returncode == 1
    assert "fastbrowse.exe" in done.stderr
    assert not (tmp_path / "package").exists()


def test_a_directory_with_something_in_it_is_not_written_over(tmp_path: Path) -> None:
    out = tmp_path / "package"
    out.mkdir()
    (out / "kept").write_text("someone's file")
    done = _assemble(_rust(tmp_path), "linux-x64", "1.2.3", out)

    assert done.returncode == 1
    assert sorted(path.name for path in out.iterdir()) == ["kept"]


def test_the_targets_are_the_ones_built_and_the_ones_the_sdk_depends_on() -> None:
    # Both jobs of the workflow build every target from one list, the Rust executable and addon and the wheel, when
    # the release asks for all of them.
    built = set(re.findall(r'\{"target": "([\w-]+)",', (REPO / ".github/workflows/binaries.yml").read_text()))
    sdk = json.loads((REPO / "packages/sdk/package.json").read_text())

    assert sorted(built) == sorted(TARGETS)
    assert sorted(sdk["optionalDependencies"]) == sorted(f"@fastbrowse/{target}" for target in TARGETS)
    for target in TARGETS:
        assert _assemble(Path("no-such-build"), target, "1.2.3", Path("unused")).stderr.startswith("refused: ")


@needs_modes
def test_the_binary_is_carried_as_fastbrowse_and_as_fastbrowse_mcp(tmp_path: Path) -> None:
    binary = _rust(tmp_path / "build")
    out = tmp_path / "package"
    manifest = _assembled(binary, "linux-x64", out)

    assert manifest["files"] == ["fastbrowse", "NOTICE"]
    assert sorted(path.name for path in (out / "fastbrowse").iterdir()) == ["fastbrowse", "fastbrowse-mcp"]
    for name in ("fastbrowse", "fastbrowse-mcp"):
        # The binary serves MCP when started under that name, and npm packs no symbolic link to give it one.
        assert not (out / "fastbrowse" / name).is_symlink()
        assert (out / "fastbrowse" / name).read_bytes() == binary.read_bytes()
        assert (out / "fastbrowse" / name).stat().st_mode & 0o111


def test_the_binary_in_the_workflow_archive_is_taken_as_it_is(tmp_path: Path) -> None:
    binary = _rust(tmp_path / "build" / "fastbrowse", name="fastbrowse.exe")
    archive = tmp_path / "fastbrowse-win32-x64.tar.gz"
    with tarfile.open(archive, "w:gz") as packed:
        packed.add(binary.parent, arcname="fastbrowse")
    out = tmp_path / "package"
    _assembled(archive, "win32-x64", out)

    assert sorted(path.name for path in (out / "fastbrowse").iterdir()) == ["fastbrowse-mcp.exe", "fastbrowse.exe"]


def test_the_addon_rides_along_as_fastbrowse_node(tmp_path: Path) -> None:
    addon = tmp_path / "libfastbrowse_node.dylib"
    addon.write_bytes(b"\xcf\xfa\xed\xfe the addon")
    out = tmp_path / "package"
    manifest = _assembled(_rust(tmp_path / "build"), "darwin-arm64", out, "--addon", str(addon))

    assert manifest["files"] == ["fastbrowse", "NOTICE", "fastbrowse.node"]
    assert (out / "fastbrowse.node").read_bytes() == addon.read_bytes()


@pytest.mark.parametrize("held", ["1.2.30", "11.2.3", "1.2"])
def test_a_binary_of_another_version_is_refused(tmp_path: Path, held: str) -> None:
    binary = tmp_path / "fastbrowse"
    binary.write_bytes(b"\x7fELF " + held.encode() + b" more")
    done = _assemble(binary, "linux-x64", "1.2.3", tmp_path / "package")

    assert done.returncode == 1
    assert "holds no version 1.2.3" in done.stderr


def test_the_rust_job_builds_the_same_targets() -> None:
    built = re.findall(r'\{"target": "([\w-]+)", "triple":', (REPO / ".github/workflows/binaries.yml").read_text())

    assert sorted(built) == sorted(TARGETS)
