"""Check that the AUR package is the PyPI release, or with `--write` make it.

    uv run python scripts/aur_package.py           # exit 1 when packaging/aur is not this version's release
    uv run python scripts/aur_package.py --write   # after the version is on PyPI

`packaging/aur/PKGBUILD` builds two source distributions from PyPI by checksum: fastbrowse, and the one exact
cdp-use version that fastbrowse release requires. Both checksums exist only once PyPI holds the release, so this runs
after a tag has published and not in the pull request that sets the version.

`.SRCINFO` is rewritten with the same values because the AUR reads it and never the PKGBUILD;
`packaging/aur/build.sh` proves the pair agree by asking makepkg.

Standard library only, so the release can run it before anything is installed.
"""

import argparse
import json
import re
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import NamedTuple

REPO = Path(__file__).resolve().parent.parent
FILES = ("PKGBUILD", ".SRCINFO")


class Release(NamedTuple):
    """What the package pins: the two versions, and the checksum of each one's source distribution."""

    version: str
    cdp_use: str
    sums: tuple[str, str]


def pinned(pkgbuild: str) -> Release:
    """The release a PKGBUILD installs."""
    version = re.search(r"^pkgver=(\S+)$", pkgbuild, re.MULTILINE)
    cdp_use = re.search(r"^_cdp_use=(\S+)$", pkgbuild, re.MULTILINE)
    sums = re.search(r"^sha256sums=\('([0-9a-f]{64})'\s+'([0-9a-f]{64})'\)$", pkgbuild, re.MULTILINE)
    if not (version and cdp_use and sums):
        raise SystemExit("PKGBUILD has no pkgver, _cdp_use or pair of sha256sums to read")
    return Release(version[1], cdp_use[1], (sums[1], sums[2]))


def rewrite(text: str, old: Release, new: Release) -> str:
    """A PKGBUILD or .SRCINFO that installed `old`, installing `new`.

    The version lines are matched whole, with or without the spaces .SRCINFO puts around `=`, and the source
    names by their own prefix: fastbrowse and cdp-use may one day carry the same number, and a plain
    replacement of one version would then move the other.
    """
    for key, was, now in (("pkgver", old.version, new.version), ("_cdp_use", old.cdp_use, new.cdp_use)):
        text = re.sub(rf"^(\s*{key} ?= ?){re.escape(was)}$", rf"\g<1>{now}", text, flags=re.MULTILINE)
    # A new upstream version starts again from the first build of it.
    text = re.sub(r"^(\s*pkgrel ?= ?)\d+$", r"\g<1>1", text, flags=re.MULTILINE)
    text = text.replace(f"fastbrowse-{old.version}.tar.gz", f"fastbrowse-{new.version}.tar.gz")
    text = text.replace(f"cdp_use-{old.cdp_use}.tar.gz", f"cdp_use-{new.cdp_use}.tar.gz")
    for was, now in zip(old.sums, new.sums, strict=True):
        text = text.replace(was, now)
    return text


def _pypi(name: str, version: str, wait: float) -> dict:
    """PyPI's record of one release. A version published a moment ago can be missing; `wait` seconds are given to it."""
    deadline = time.monotonic() + wait
    while True:
        try:
            with urllib.request.urlopen(f"https://pypi.org/pypi/{name}/{version}/json", timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            if time.monotonic() >= deadline:
                raise SystemExit(f"{name} {version} is not on PyPI") from error
            time.sleep(10)


def _source_sum(record: dict) -> str:
    sources = [file for file in record["urls"] if file["packagetype"] == "sdist"]
    if len(sources) != 1:
        raise SystemExit(f"{record['info']['name']} {record['info']['version']} has no single source distribution")
    return sources[0]["digests"]["sha256"]


def published(version: str, wait: float = 0) -> Release:
    """The release as PyPI holds it: the cdp-use version it requires, and both sources' checksums."""
    record = _pypi("fastbrowse", version, wait)
    requires = record["info"]["requires_dist"] or []
    pins = [match[1] for line in requires if (match := re.fullmatch(r"cdp-use==(\S+)", line))]
    if len(pins) != 1:
        raise SystemExit(f"fastbrowse {version} does not require one exact cdp-use version, which the PKGBUILD assumes")
    return Release(version, pins[0], (_source_sum(record), _source_sum(_pypi("cdp-use", pins[0], wait))))


def main() -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("version", nargs="?", help="the PyPI release to package; the one in pyproject.toml by default")
    parser.add_argument("--root", type=Path, default=REPO, help="the checkout to read, this one by default")
    parser.add_argument("--write", action="store_true", help="set the package to the PyPI release")
    parser.add_argument("--wait", type=float, default=0, metavar="SECONDS", help="how long to wait for PyPI to list it")
    args = parser.parse_args()
    project = tomllib.loads((args.root / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    version = args.version or project["version"]
    directory = args.root / "packaging" / "aur"
    old = pinned((directory / "PKGBUILD").read_text(encoding="utf-8"))
    new = published(version, args.wait)
    if old == new:
        print(f"the AUR package is fastbrowse {new.version} with cdp-use {new.cdp_use}")
        return 0
    if not args.write:
        print(f"packaging/aur is fastbrowse {old.version} with cdp-use {old.cdp_use}, and PyPI has:", file=sys.stderr)
        print(f"  fastbrowse {new.version} with cdp-use {new.cdp_use}", file=sys.stderr)
        print("run scripts/aur_package.py --write", file=sys.stderr)
        return 1
    for name in FILES:
        path = directory / name
        path.write_text(rewrite(path.read_text(encoding="utf-8"), old, new), encoding="utf-8")
    print(f"the AUR package is now fastbrowse {new.version} with cdp-use {new.cdp_use}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
