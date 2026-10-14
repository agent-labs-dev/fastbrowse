"""Fail the build on typographic punctuation: em and en dashes, and curly quotes.

A house convention: prose here is plain ASCII, so write a hyphen, a comma, a colon, or two sentences.
This checks every tracked text file, code and data included, which Vale cannot: Vale (`.vale.ini`) owns
the wording rules and reads only prose in Markdown and in Python comments and docstrings.

Escape one deliberately with `slop-ok: <reason>` on the same line, and say why it has to stay.

    uv run python scripts/no_slop.py [PATH ...]
"""

import re
import subprocess
import sys
from pathlib import Path

SUFFIXES = frozenset({".py", ".md", ".toml", ".yml", ".yaml", ".html", ".sh", ".txt", ".json", ".cfg"})
# The Arch package's two files, which have no suffix to be known by.
NAMES = frozenset({"PKGBUILD", ".SRCINFO"})

# Built from code points so this table is not itself a line of the punctuation it bans.
CHARACTERS = {
    chr(0x2014): "em dash",
    chr(0x2013): "en dash",
    chr(0x2015): "horizontal bar",
    chr(0x201C): "curly double quote",
    chr(0x201D): "curly double quote",
    chr(0x2018): "curly single quote",
    chr(0x2019): "curly apostrophe",
}

ALLOW = re.compile(r"slop-ok:\s*\S")


def tracked(roots: list[str]) -> list[Path]:
    listing = subprocess.run(["git", "ls-files", "-z", *roots], capture_output=True, text=True, check=True)
    paths = [Path(name) for name in listing.stdout.split("\0") if name]
    return [path for path in paths if path.suffix in SUFFIXES or path.name in NAMES]


def offences(path: Path) -> list[str]:
    found: list[str] = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if ALLOW.search(line):
            continue
        for column, character in enumerate(line, start=1):
            if name := CHARACTERS.get(character):
                found.append(f"{path}:{number}:{column}: {name} ({character!r})")
    return found


def main(argv: list[str]) -> int:
    found = [offence for path in tracked(argv) for offence in offences(path)]
    for offence in found:
        print(offence)
    if found:
        print(f"\n{len(found)} found. Rewrite them, or annotate the line with `slop-ok: <reason>`.")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
