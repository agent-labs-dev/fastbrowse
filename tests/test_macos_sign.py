"""`scripts/macos_sign.py`: sign, notarize and verify the Mach-O files of one frozen build.

Apple's tools are stood in for by programs of the same names on the PATH, which write down how they were called
and answer as told. That shows what the script asks of them on any system. The last tests use the real `codesign`
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
SCRIPT = REPO / "scripts" / "macos_sign.py"

pytestmark = pytest.mark.skipif(os.name == "nt", reason="the stand-ins are started by their first line")

MACH_O = b"\xcf\xfa\xed\xfe" + b"\0" * 28

# Each stand-in appends its name and arguments to $CALLS. `codesign -d` and `notarytool` answer with what the
# test left in the environment, and a call whose arguments contain $REFUSE exits 1.
STAND_IN = f"""#!{sys.executable}
import json, os, sys
name, arguments = os.path.basename(sys.argv[0]), sys.argv[1:]
with open(os.environ["CALLS"], "a") as calls:
    calls.write(json.dumps([name, *arguments]) + "\\n")
refuse = os.environ.get("REFUSE")
if refuse and any(refuse in argument for argument in arguments):
    sys.exit(1)
if name == "codesign" and "-d" in arguments:
    sys.stderr.write(os.environ.get("DISPLAY_SAYS", ""))
if name == "xcrun" and arguments[:2] == ["notarytool", "submit"]:
    print(os.environ["SUBMIT_SAYS"])
if name == "xcrun" and arguments[:2] == ["notarytool", "log"]:
    print("the log of the submission")
"""

DEVELOPER_ID = """CodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+2 location=embedded
Authority=Developer ID Application: Agent Labs (TEAM123456)
Authority=Developer ID Certification Authority
Timestamp=Oct 5, 2026 at 12:00:00 PM
TeamIdentifier=TEAM123456
"""

CREDENTIALS = {"APPLE_ID": "dev@example.com", "APPLE_TEAM_ID": "TEAM123456", "APPLE_APP_SPECIFIC_PASSWORD": "abcd-efgh"}


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


class Tools:
    """The stand-ins on a PATH of their own, and the calls they have had."""

    def __init__(self, root: Path) -> None:
        self._calls = root / "calls"
        self._calls.touch()
        bin = root / "bin"
        bin.mkdir()
        for name in ("codesign", "xcrun", "ditto"):
            (bin / name).write_text(STAND_IN)
            (bin / name).chmod(0o755)
        self.env = {"PATH": f"{bin}{os.pathsep}{os.environ['PATH']}", "CALLS": str(self._calls)}

    def run(self, *arguments: str, **env: str) -> subprocess.CompletedProcess[str]:
        command = [sys.executable, str(SCRIPT), *arguments]
        return subprocess.run(command, capture_output=True, text=True, env=self.env | env)

    def calls(self, name: str) -> list[list[str]]:
        lines = [json.loads(line) for line in self._calls.read_text().splitlines()]
        return [line[1:] for line in lines if line[0] == name]


@pytest.fixture
def tools(tmp_path: Path) -> Tools:
    return Tools(tmp_path)


def _names(tree: Path, calls: list[list[str]]) -> list[str]:
    return [Path(call[-1]).relative_to(tree).as_posix() for call in calls]


def test_every_mach_o_file_is_signed_once_and_the_executable_last(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    done = tools.run("sign", str(tree), "--identity", "Developer ID Application: Agent Labs (TEAM123456)")

    assert done.returncode == 0, done.stderr
    # A link is the file it points to, and signing it again would sign that file twice.
    assert _names(tree, tools.calls("codesign")) == [
        "_internal/libpython3.13.dylib",
        "_internal/pydantic_core/_pydantic_core.cpython-313-darwin.so",
        "fastbrowse",
    ]


def test_a_release_identity_signs_with_what_notarization_requires(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    tools.run("sign", str(tree), "--identity", "ABCDEF0123", "--keychain", "/tmp/signing.keychain-db")

    calls = tools.calls("codesign")
    assert len(calls) == 3
    for call in calls:
        assert call[:-1] == [
            "--force",
            "--sign",
            "ABCDEF0123",
            "--keychain",
            "/tmp/signing.keychain-db",
            "--options",
            "runtime",
            "--timestamp",
        ]


def test_the_ad_hoc_identity_signs_without_the_hardened_runtime(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    tools.run("sign", str(tree), "--identity", "-")

    calls = tools.calls("codesign")
    assert len(calls) == 3
    for call in calls:
        assert call[:-1] == ["--force", "--sign", "-", "--timestamp=none"]


def test_a_file_that_cannot_be_signed_fails_the_command(tmp_path: Path, tools: Tools) -> None:
    done = tools.run("sign", str(_frozen(tmp_path)), "--identity", "-", REFUSE="libpython")

    assert done.returncode == 1
    assert "libpython3.13.dylib" in done.stderr


def test_a_directory_that_is_not_a_build_is_refused(tmp_path: Path, tools: Tools) -> None:
    (tmp_path / "empty").mkdir()

    done = tools.run("sign", str(tmp_path / "empty"), "--identity", "-")

    assert done.returncode == 1
    assert "fastbrowse" in done.stderr
    assert tools.calls("codesign") == []


def test_notarize_submits_an_archive_of_the_build_and_passes_when_it_is_accepted(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    done = tools.run("notarize", str(tree), **CREDENTIALS, SUBMIT_SAYS='{"id": "sub-1", "status": "Accepted"}')

    assert done.returncode == 0, done.stderr
    (archived,) = tools.calls("ditto")
    assert archived[:-1] == ["-c", "-k", "--keepParent", str(tree)]
    (submitted,) = tools.calls("xcrun")
    assert submitted[:3] == ["notarytool", "submit", archived[-1]]
    assert archived[-1].endswith(".zip")
    assert "--wait" in submitted
    for flag, value in (("--apple-id", "dev@example.com"), ("--team-id", "TEAM123456"), ("--password", "abcd-efgh")):
        assert submitted[submitted.index(flag) + 1] == value
    assert "abcd-efgh" not in done.stdout + done.stderr


def test_a_submission_apple_does_not_accept_fails_and_shows_its_log(tmp_path: Path, tools: Tools) -> None:
    # notarytool exits 0 for a submission it processed, whatever the verdict.
    done = tools.run(
        "notarize", str(_frozen(tmp_path)), **CREDENTIALS, SUBMIT_SAYS='{"id": "sub-2", "status": "Invalid"}'
    )

    assert done.returncode == 1
    assert "Invalid" in done.stderr
    assert "the log of the submission" in done.stderr
    assert tools.calls("xcrun")[-1][:3] == ["notarytool", "log", "sub-2"]


def test_notarize_names_the_credential_that_is_not_set(tmp_path: Path, tools: Tools) -> None:
    done = tools.run("notarize", str(_frozen(tmp_path)), APPLE_ID="dev@example.com", APPLE_TEAM_ID="TEAM123456")

    assert done.returncode == 1
    assert "APPLE_APP_SPECIFIC_PASSWORD" in done.stderr
    assert tools.calls("xcrun") == []


def test_verify_checks_every_mach_o_file_strictly(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    done = tools.run("verify", str(tree))

    assert done.returncode == 0, done.stderr
    calls = tools.calls("codesign")
    assert len(calls) == 3
    assert all(call[:2] == ["--verify", "--strict"] for call in calls)


def test_verify_fails_on_a_file_whose_signature_does_not_hold(tmp_path: Path, tools: Tools) -> None:
    done = tools.run("verify", str(_frozen(tmp_path)), REFUSE="_pydantic_core")

    assert done.returncode == 1
    assert "_pydantic_core.cpython-313-darwin.so" in done.stderr


def test_verify_notarized_asks_apple_about_every_file(tmp_path: Path, tools: Tools) -> None:
    tree = _frozen(tmp_path)

    done = tools.run("verify", str(tree), "--notarized", DISPLAY_SAYS=DEVELOPER_ID)

    assert done.returncode == 0, done.stderr
    asked = [call for call in tools.calls("codesign") if "--check-notarization" in call]
    assert len(asked) == 3
    assert all("-R=notarized" in call for call in asked)


@pytest.mark.parametrize(
    ("missing", "said"),
    [
        ("Authority=Developer ID Application: Agent Labs (TEAM123456)\n", "Developer ID"),
        ("(runtime)", "hardened runtime"),
        ("Timestamp=Oct 5, 2026 at 12:00:00 PM\n", "timestamp"),
    ],
)
def test_verify_notarized_fails_on_a_signature_notarization_would_refuse(
    tmp_path: Path, tools: Tools, missing: str, said: str
) -> None:
    done = tools.run("verify", str(_frozen(tmp_path)), "--notarized", DISPLAY_SAYS=DEVELOPER_ID.replace(missing, ""))

    assert done.returncode == 1
    assert said in done.stderr


needs_codesign = pytest.mark.skipif(
    sys.platform != "darwin" or shutil.which("codesign") is None, reason="needs macOS's codesign"
)


def _real(root: Path) -> Path:
    """A build whose two Mach-O files are copies of a real one. They are there to be signed, and are never run."""
    tree = root / "fastbrowse"
    (tree / "_internal").mkdir(parents=True)
    shutil.copyfile("/bin/echo", tree / "fastbrowse")
    shutil.copyfile("/bin/echo", tree / "_internal" / "libpython3.13.dylib")
    (tree / "fastbrowse").chmod(0o755)
    return tree


def _script(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run([sys.executable, str(SCRIPT), *arguments], capture_output=True, text=True)


@needs_codesign
def test_an_ad_hoc_signed_build_verifies(tmp_path: Path) -> None:
    tree = _real(tmp_path)

    signed = _script("sign", str(tree), "--identity", "-")
    assert signed.returncode == 0, signed.stderr

    verified = _script("verify", str(tree))
    assert verified.returncode == 0, verified.stderr


@needs_codesign
def test_a_file_changed_after_signing_fails_verification(tmp_path: Path) -> None:
    tree = _real(tmp_path)
    _script("sign", str(tree), "--identity", "-")
    library = tree / "_internal" / "libpython3.13.dylib"
    changed = bytearray(library.read_bytes())
    changed[len(changed) // 2] ^= 0xFF
    library.write_bytes(changed)

    done = _script("verify", str(tree))

    assert done.returncode == 1
    assert "libpython3.13.dylib" in done.stderr


@needs_codesign
def test_an_ad_hoc_signed_build_is_not_a_notarized_one(tmp_path: Path) -> None:
    tree = _real(tmp_path)
    _script("sign", str(tree), "--identity", "-")

    done = _script("verify", str(tree), "--notarized")

    assert done.returncode == 1
    assert "Developer ID" in done.stderr
