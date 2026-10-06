"""Sign, notarize and verify one frozen macOS build, the directory PyInstaller wrote.

    python scripts/macos_sign.py sign dist/fastbrowse --identity "Developer ID Application: ..." [--keychain PATH]
    python scripts/macos_sign.py notarize dist/fastbrowse
    python scripts/macos_sign.py verify dist/fastbrowse --notarized

The build is a directory and not an app bundle, so nothing seals it as a whole: each Mach-O file in it carries
its own signature, and each is signed, checked and looked up with Apple by itself.

`notarize` reads `APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_SPECIFIC_PASSWORD` from the environment. Apple keeps
the ticket: `stapler` attaches one to an app, a package or a disk image and to nothing else, so `verify
--notarized` asks Apple about each file where a bundle would be checked with `stapler validate`.

`--identity -` signs ad hoc, with no certificate. That is for trying the script on a build of your own. Such a
build is not a release: `verify` passes on it and `verify --notarized` does not.

Standard library only: it runs on a build runner with nothing installed.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

# How a Mach-O file starts: 32 and 64 bit in either byte order, then a file that holds more than one architecture.
MAGICS = (b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe")

CREDENTIALS = ("APPLE_ID", "APPLE_TEAM_ID", "APPLE_APP_SPECIFIC_PASSWORD")

AD_HOC = "-"

# Apple answers within minutes. A wait with no end would hold a release until the job itself ran out of time.
WAIT = "30m"


class Failed(Exception):
    """A step that did not hold, with what was seen instead."""


def mach_o_files(tree: Path) -> list[Path]:
    """Every file in the build that carries a signature, the executable last.

    Found by how a file starts and not by its name: PyInstaller collects libraries under names with no suffix.
    The executable is last because that is the order Apple asks for, the code a program loads before the
    program. A symbolic link is left out, since the file it points to is in the list already.
    """
    executable = tree / "fastbrowse"
    if not executable.is_file() or not (tree / "_internal").is_dir():
        raise Failed(f"{tree} is not a macOS build: it needs fastbrowse and _internal/ beside it")
    found = []
    for path in sorted((tree / "_internal").rglob("*")):
        if path.is_symlink() or not path.is_file():
            continue
        with path.open("rb") as file:
            if file.read(4) in MAGICS:
                found.append(path)
    return [*found, executable]


def _codesign(*arguments: str, path: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["codesign", *arguments, str(path)], capture_output=True, text=True)


def sign(tree: Path, identity: str, keychain: str | None) -> int:
    files = mach_o_files(tree)
    if identity == AD_HOC:
        # No hardened runtime: under it a program loads only libraries signed by its own team, and an ad-hoc
        # signature names no team, so the executable would refuse the Python library beside it.
        how = ["--timestamp=none"]
    else:
        # Notarization refuses a file without the hardened runtime or without Apple's timestamp.
        how = [*(["--keychain", keychain] if keychain else []), "--options", "runtime", "--timestamp"]
    for path in files:
        # `--force` replaces the ad-hoc signature PyInstaller gives every file it collects.
        done = _codesign("--force", "--sign", identity, *how, path=path)
        if done.returncode != 0:
            raise Failed(f"codesign could not sign {path}:\n{done.stderr.strip()}")
    return len(files)


def notarize(tree: Path) -> str:
    """Send the build to Apple's notary service and return the id of the accepted submission."""
    mach_o_files(tree)
    unset = [name for name in CREDENTIALS if not os.environ.get(name)]
    if unset:
        raise Failed(f"notarization needs {', '.join(unset)} in the environment")
    apple_id, team_id, password = (os.environ[name] for name in CREDENTIALS)
    account = ["--apple-id", apple_id, "--team-id", team_id, "--password", password]
    with tempfile.TemporaryDirectory() as temporary:
        # The service takes an archive, a package or a disk image, and never a directory.
        archive = Path(temporary) / "fastbrowse.zip"
        zipped = subprocess.run(["ditto", "-c", "-k", "--keepParent", str(tree), str(archive)], capture_output=True)
        if zipped.returncode != 0:
            raise Failed(f"ditto could not archive {tree}:\n{zipped.stderr.decode(errors='replace').strip()}")
        submitted = subprocess.run(
            ["xcrun", "notarytool", "submit", str(archive), "--wait", "--timeout", WAIT, "-f", "json", *account],
            capture_output=True,
            text=True,
        )
    try:
        verdict = json.loads(submitted.stdout)
    except ValueError:
        verdict = {}
    if submitted.returncode == 0 and verdict.get("status") == "Accepted":
        return str(verdict["id"])
    # notarytool exits 0 for a submission it processed, whatever the verdict, so the verdict is what is read.
    said = f"notarytool exited with status {submitted.returncode}:\n{submitted.stdout}{submitted.stderr}".strip()
    if verdict.get("id"):
        # What Apple found wrong, file by file.
        log = subprocess.run(
            ["xcrun", "notarytool", "log", str(verdict["id"]), *account], capture_output=True, text=True
        )
        said += f"\n{log.stdout}{log.stderr}".rstrip()
    raise Failed(f"the submission was not accepted (status {verdict.get('status')!r}). {said}")


def _release_signature(path: Path) -> list[str]:
    """What the signature of this file lacks for notarization, read from what `codesign` displays of it."""
    shown = _codesign("-d", "--verbose=4", path=path).stderr
    lines = shown.splitlines()
    lacks = []
    if not any(line.startswith("Authority=Developer ID Application:") for line in lines):
        lacks.append("a Developer ID Application certificate")
    if not any(line.startswith("CodeDirectory ") and "runtime" in line for line in lines):
        lacks.append("the hardened runtime")
    if not any(line.startswith("Timestamp=") for line in lines):
        lacks.append("a secure timestamp")
    return lacks


def verify(tree: Path, notarized: bool) -> int:
    files = mach_o_files(tree)
    problems = []
    for path in files:
        valid = _codesign("--verify", "--strict", "--verbose=2", path=path)
        if valid.returncode != 0:
            problems.append(f"{path}: the signature does not hold: {valid.stderr.strip()}")
            continue
        if not notarized:
            continue
        lacks = _release_signature(path)
        if lacks:
            problems.append(f"{path}: the signature lacks {', '.join(lacks)}")
            continue
        # Passes only when Apple, asked now, holds a ticket for this file.
        ticket = _codesign("--verify", "--strict", "-R=notarized", "--check-notarization", path=path)
        if ticket.returncode != 0:
            problems.append(f"{path}: Apple holds no notarization ticket for it: {ticket.stderr.strip()}")
    if problems:
        raise Failed("\n".join(problems))
    return len(files)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    signing = commands.add_parser("sign", help="sign every Mach-O file of the build")
    signing.add_argument("tree", type=Path)
    signing.add_argument("--identity", required=True, help="a certificate's name or SHA-1, or - to sign ad hoc")
    signing.add_argument("--keychain", help="the keychain that holds the certificate")
    notarizing = commands.add_parser("notarize", help="submit the signed build to Apple and wait for the verdict")
    notarizing.add_argument("tree", type=Path)
    verifying = commands.add_parser("verify", help="check the signature of every Mach-O file of the build")
    verifying.add_argument("tree", type=Path)
    verifying.add_argument("--notarized", action="store_true", help="also require what a release needs")
    args = parser.parse_args()
    try:
        if args.command == "sign":
            print(f"signed {sign(args.tree, args.identity, args.keychain)} Mach-O files in {args.tree}")
        elif args.command == "notarize":
            print(f"accepted: submission {notarize(args.tree)}")
        else:
            held = "signed by a Developer ID and notarized" if args.notarized else "validly signed"
            print(f"{verify(args.tree, args.notarized)} Mach-O files in {args.tree} are {held}")
    except Failed as exc:
        print(f"FAILED: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
