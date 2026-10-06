"""Check that every Mach-O file of one frozen macOS build carries a signature that holds.

    python scripts/macos_signatures.py dist/fastbrowse

The build is not signed with a certificate and not notarized. PyInstaller signs each file it collects ad hoc,
and that is what is checked here: a Mac with Apple silicon runs no code without a signature, so a file that
lost its own stops the program at the moment it is loaded.

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
    """Every file in the build that carries a signature.

    Found by how a file starts and not by its name: PyInstaller collects libraries under names with no suffix.
    A symbolic link is left out, since the file it points to is in the list already.
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


def verify(tree: Path) -> int:
    files = mach_o_files(tree)
    problems = []
    for path in files:
        valid = subprocess.run(
            ["codesign", "--verify", "--strict", "--verbose=2", str(path)], capture_output=True, text=True
        )
        if valid.returncode != 0:
            problems.append(f"{path}: the signature does not hold: {valid.stderr.strip()}")
    if problems:
        raise Failed("\n".join(problems))
    return len(files)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tree", type=Path)
    args = parser.parse_args()
    try:
        print(f"{verify(args.tree)} Mach-O files in {args.tree} are validly signed")
    except Failed as exc:
        print(f"FAILED: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
