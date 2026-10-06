"""`scripts/macos_signatures.py`: check the signature of every Mach-O file of one frozen build.

`codesign` is stood in for by a program of the same name on the PATH, which writes down how it was called and
answers as told. That shows what the script asks of it on any system. The last tests use the real `codesign`
with an ad-hoc identity, and run on macOS only.
"""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "macos_signatures.py"

pytestmark = pytest.mark.skipif(os.name == "nt", reason="the stand-in is started by its first line")

MACH_O = b"\xcf\xfa\xed\xfe" + b"\0" * 28

# The stand-in appends its arguments to $CALLS, and a call whose arguments contain $REFUSE exits 1.
STAND_IN = f"""#!{sys.executable}
import json, os, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as calls:
    calls.write(json.dumps(arguments) + "\\n")
refuse = os.environ.get("REFUSE")
if refuse and any(refuse in argument for argument in arguments):
    sys.exit(1)
"""


def _frozen(root: Path) -> Path:
    """A directory shaped like PyInstaller's output, with files that start as Mach-O files do."""
    tree = root / "fastbrowse"
    internal = tree / "_internal"
    (internal / "pydantic_core").mkdir(parents=True)
    (internal / "pydantic_core" / "_pydantic_core.cpython-313-darwin.so").write_bytes(MACH_O)
    (internal / "libpython3.13.dylib").write_bytes(MACH_O)
    (internal / "Python").symlink_to("libpython3.13.dylib")
    (internal / "base_library.zip").write_bytes(b"PK\x03\x04")
    (internal / "snapshot.js").write_text("// the page script\n")
    (internal / "empty").write_bytes(b"")
    (tree / "fastbrowse").write_bytes(MACH_O)
    return tree


class Codesign:
    """The stand-in on a PATH of its own, and the calls it has had."""

    def __init__(self, root: Path) -> None:
        self._calls = root / "calls"
        self._calls.touch()
        bin = root / "bin"
        bin.mkdir()
        (bin / "codesign").write_text(STAND_IN)
        (bin / "codesign").chmod(0o755)
        self.env = {"PATH": f"{bin}{os.pathsep}{os.environ['PATH']}", "CALLS": str(self._calls)}

    def run(self, *arguments: str, **env: str) -> subprocess.CompletedProcess[str]:
        command = [sys.executable, str(SCRIPT), *arguments]
        return subprocess.run(command, capture_output=True, text=True, env=self.env | env)

    def calls(self) -> list[list[str]]:
        return [json.loads(line) for line in self._calls.read_text().splitlines()]


@pytest.fixture
def codesign(tmp_path: Path) -> Codesign:
    return Codesign(tmp_path)


def test_every_mach_o_file_is_checked_once_and_strictly(tmp_path: Path, codesign: Codesign) -> None:
    tree = _frozen(tmp_path)

    done = codesign.run(str(tree))

    assert done.returncode == 0, done.stderr
    calls = codesign.calls()
    # A link is the file it points to, and a file that is no Mach-O file carries no signature to check.
    assert [Path(call[-1]).relative_to(tree).as_posix() for call in calls] == [
        "_internal/libpython3.13.dylib",
        "_internal/pydantic_core/_pydantic_core.cpython-313-darwin.so",
        "fastbrowse",
    ]
    assert all(call[:2] == ["--verify", "--strict"] for call in calls)


def test_a_file_whose_signature_does_not_hold_fails_the_command(tmp_path: Path, codesign: Codesign) -> None:
    done = codesign.run(str(_frozen(tmp_path)), REFUSE="_pydantic_core")

    assert done.returncode == 1
    assert "_pydantic_core.cpython-313-darwin.so" in done.stderr


def test_a_directory_that_is_not_a_build_is_refused(tmp_path: Path, codesign: Codesign) -> None:
    (tmp_path / "fastbrowse").mkdir()

    done = codesign.run(str(tmp_path / "fastbrowse"))

    assert done.returncode == 1
    assert "not a macOS build" in done.stderr
    assert codesign.calls() == []


needs_codesign = pytest.mark.skipif(
    sys.platform != "darwin" or shutil.which("codesign") is None, reason="needs macOS's codesign"
)


def _ad_hoc(root: Path) -> Path:
    """A build whose two Mach-O files are copies of a real one, signed as PyInstaller signs. They are never run."""
    tree = root / "fastbrowse"
    (tree / "_internal").mkdir(parents=True)
    for path in (tree / "_internal" / "libpython3.13.dylib", tree / "fastbrowse"):
        shutil.copyfile("/bin/echo", path)
        subprocess.run(["codesign", "--force", "--sign", "-", "--timestamp=none", str(path)], check=True)
    (tree / "fastbrowse").chmod(0o755)
    return tree


def _script(tree: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run([sys.executable, str(SCRIPT), str(tree)], capture_output=True, text=True)


@needs_codesign
def test_an_ad_hoc_signed_build_passes(tmp_path: Path) -> None:
    done = _script(_ad_hoc(tmp_path))

    assert done.returncode == 0, done.stderr


@needs_codesign
def test_a_file_changed_after_signing_fails(tmp_path: Path) -> None:
    tree = _ad_hoc(tmp_path)
    library = tree / "_internal" / "libpython3.13.dylib"
    changed = bytearray(library.read_bytes())
    changed[len(changed) // 2] ^= 0xFF
    library.write_bytes(changed)

    done = _script(tree)

    assert done.returncode == 1
    assert "libpython3.13.dylib" in done.stderr
