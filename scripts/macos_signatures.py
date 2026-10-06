"""Sign every Mach-O file of one frozen macOS build ad hoc, and check that each signature holds.

    python scripts/macos_signatures.py sign dist/fastbrowse
    python scripts/macos_signatures.py verify dist/fastbrowse

The build is not signed with a certificate and not notarized. A Mac with Apple silicon runs no code without a
signature, and an ad-hoc one, which names no certificate, is what it gets here.

PyInstaller signs what it collects, and still the build is signed again. The Python library comes out of a
framework, and it keeps the signature it had there, which seals files the build does not carry. The Mac loads
it all the same, and `codesign --verify` calls it broken, as would any tool that checks a program before
allowing it.

An ad-hoc signature is enough for a build installed through npm. Gatekeeper assesses a file only when it
carries the quarantine attribute, which a browser sets on what it downloads and npm does not set on what it
unpacks. A Mac whose administrator allows programs by the team that signed them refuses this build.

The build is a directory and not an app bundle, so nothing seals it as a whole: each Mach-O file in it carries
its own signature, and each is checked by itself.

Standard library only: it runs on a build runner with nothing installed.
"""

import argparse
import subprocess
import sys
from pathlib import Path

# How a Mach-O file starts: 32 and 64 bit in either byte order, then a file that holds more than one architecture.
MAGICS = (b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe")


class Failed(Exception):
    """A check that did not hold, with what was seen instead."""


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


def sign(tree: Path) -> int:
    files = mach_o_files(tree)
    for path in files:
        # No hardened runtime: under it a program loads only libraries signed by its own team, and an ad-hoc
        # signature names no team, so the executable would refuse the Python library beside it.
        done = _codesign("--force", "--sign", "-", "--timestamp=none", path=path)
        if done.returncode != 0:
            raise Failed(f"codesign could not sign {path}:\n{done.stderr.strip()}")
    return len(files)


def verify(tree: Path) -> int:
    files = mach_o_files(tree)
    problems = []
    for path in files:
        valid = _codesign("--verify", "--strict", "--verbose=2", path=path)
        if valid.returncode != 0:
            problems.append(f"{path}: the signature does not hold: {valid.stderr.strip()}")
    if problems:
        raise Failed("\n".join(problems))
    return len(files)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("sign", help="sign every Mach-O file of the build ad hoc").add_argument("tree", type=Path)
    commands.add_parser("verify", help="check the signature of every Mach-O file").add_argument("tree", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "sign":
            print(f"signed {sign(args.tree)} Mach-O files in {args.tree} ad hoc")
        else:
            print(f"{verify(args.tree)} Mach-O files in {args.tree} are validly signed")
    except Failed as exc:
        print(f"FAILED: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
