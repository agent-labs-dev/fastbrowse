"""Check that the AUR package is the PyPI release, or with `--write` make it.

    uv run python scripts/aur_package.py           # exit 1 when packaging/aur is not this version's release
    uv run python scripts/aur_package.py --write   # after the version is on PyPI

    uv run python scripts/aur_package.py --publish   # after the package has built, put it on the AUR

`packaging/aur/PKGBUILD` builds two source distributions from PyPI by checksum: fastbrowse, and the one exact
cdp-use version that fastbrowse release requires. Both checksums exist only once PyPI holds the release, so this runs
after a tag has published and not in the pull request that sets the version.

A release used to fail here after PyPI had accepted the upload but before its file host served the file: the
checksums were right and the Arch build's download was a 404. `--wait` therefore waits for the file itself, not
only for PyPI's record of it, so a release costs no retry.

`.SRCINFO` is rewritten with the same values because the AUR reads it and never the PKGBUILD;
`packaging/aur/build.sh` proves the pair agree by asking makepkg.

Standard library only, so the release can run it before anything is installed.
"""

import argparse
import json
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import NamedTuple

REPO = Path(__file__).resolve().parent.parent
FILES = ("PKGBUILD", ".SRCINFO")
# The AUR's git endpoint for the package. Pushing needs a key the package maintainer registered with the AUR;
# nothing else here talks to it.
AUR_REMOTE = "ssh://aur@aur.archlinux.org/fastbrowse.git"


# The newest mcp 1.x. Arch's python-mcp moved to 2.x, and mcp 2 renamed FastMCP to MCPServer, so the Python
# core's MCP server cannot import it. fastbrowse requires a range (`mcp<2,>=1.20`) rather than one exact
# version, so this cannot be read off PyPI the way cdp-use's exact pin is: it is the newest 1.x chosen here.
MCP = "1.30.0"


class Release(NamedTuple):
    """What the package pins: the three versions, and the checksum of each one's source distribution."""

    version: str
    cdp_use: str
    mcp: str
    sums: tuple[str, str, str]


def pinned(pkgbuild: str) -> Release:
    """The release a PKGBUILD installs."""
    version = re.search(r"^pkgver=(\S+)$", pkgbuild, re.MULTILINE)
    cdp_use = re.search(r"^_cdp_use=(\S+)$", pkgbuild, re.MULTILINE)
    mcp = re.search(r"^_mcp=(\S+)$", pkgbuild, re.MULTILINE)
    sums = re.search(r"^sha256sums=\('([0-9a-f]{64})'\s+'([0-9a-f]{64})'\s+'([0-9a-f]{64})'\)$", pkgbuild, re.MULTILINE)
    if not (version and cdp_use and mcp and sums):
        raise SystemExit("PKGBUILD has no pkgver, _cdp_use, _mcp or triple of sha256sums to read")
    return Release(version[1], cdp_use[1], mcp[1], (sums[1], sums[2], sums[3]))


def rewrite(text: str, old: Release, new: Release) -> str:
    """A PKGBUILD or .SRCINFO that installed `old`, installing `new`.

    The version lines are matched whole, with or without the spaces .SRCINFO puts around `=`, and the source
    names by their own prefix: fastbrowse and cdp-use may one day carry the same number, and a plain
    replacement of one version would then move the other.
    """
    for key, was, now in (
        ("pkgver", old.version, new.version),
        ("_cdp_use", old.cdp_use, new.cdp_use),
        ("_mcp", old.mcp, new.mcp),
    ):
        text = re.sub(rf"^(\s*{key} ?= ?){re.escape(was)}$", rf"\g<1>{now}", text, flags=re.MULTILINE)
    # A new upstream version starts again from the first build of it.
    text = re.sub(r"^(\s*pkgrel ?= ?)\d+$", r"\g<1>1", text, flags=re.MULTILINE)
    text = text.replace(f"fastbrowse-{old.version}.tar.gz", f"fastbrowse-{new.version}.tar.gz")
    text = text.replace(f"cdp_use-{old.cdp_use}.tar.gz", f"cdp_use-{new.cdp_use}.tar.gz")
    text = text.replace(f"mcp-{old.mcp}.tar.gz", f"mcp-{new.mcp}.tar.gz")
    for was, now in zip(old.sums, new.sums, strict=True):
        text = text.replace(was, now)
    return text


def source_url(name: str, filename: str) -> str:
    """The URL a PKGBUILD names a source distribution by.

    PyPI's JSON also carries a hashed path for the same file, but the package is written with this stable one,
    so this is the address that has to answer before a build of it can start.
    """
    return f"https://files.pythonhosted.org/packages/source/{name[0]}/{name}/{filename}"


def _reachable(url: str, wait: float) -> None:
    """Wait until `url` serves the file, not merely until PyPI lists it.

    An upload is accepted before PyPI's file host serves it everywhere, and the seconds in between are enough for
    `makepkg` to fetch a 404 and fail the release. `wait` seconds are given to the file itself, so the release
    builds only what it can download.
    """
    deadline = time.monotonic() + wait
    while True:
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                response.read(0)
                return
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            if time.monotonic() >= deadline:
                raise SystemExit(f"{url} is not served yet") from error
            time.sleep(10)
        except (urllib.error.URLError, TimeoutError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(10)


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
    release = Release(
        version,
        pins[0],
        MCP,
        (_source_sum(record), _source_sum(_pypi("cdp-use", pins[0], wait)), _source_sum(_pypi("mcp", MCP, wait))),
    )
    if wait:
        # The three files the PKGBUILD will fetch, waited for by the address it fetches them from.
        for name, pinned_version in (("fastbrowse", version), ("cdp-use", pins[0]), ("mcp", MCP)):
            _reachable(source_url(name, f"{name.replace('-', '_')}-{pinned_version}.tar.gz"), wait)
    return release


def changed(current: dict[str, str], wanted: dict[str, str]) -> list[str]:
    """The files whose bytes differ from what the AUR holds, in `FILES` order.

    An AUR commit that rewrote a file with the bytes it already had would be a new revision of the package that
    changed nothing, and the AUR shows that revision to everyone who watches it.
    """
    return [name for name in FILES if current.get(name) != wanted.get(name)]


def _git(*args: str, cwd: Path, env: dict[str, str] | None = None) -> str:
    done = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=False, env=env)
    if done.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} failed in {cwd}: {done.stderr.strip()}")
    return done.stdout


def publish(directory: Path, release: Release, remote: str = AUR_REMOTE) -> int:
    """Put `directory`'s PKGBUILD and .SRCINFO on the AUR, or say the AUR already holds them.

    The key that authenticates this has to be one the package's AUR maintainer registered, so this is the one
    step of a release that cannot run without a secret. `GIT_SSH_COMMAND` from the caller supplies it.
    """
    wanted = {name: (directory / name).read_text(encoding="utf-8") for name in FILES}
    with tempfile.TemporaryDirectory(prefix="aur-") as scratch:
        work = Path(scratch) / "fastbrowse"
        _git("clone", "--quiet", "--depth", "1", remote, str(work), cwd=Path(scratch))
        current = {name: (work / name).read_text(encoding="utf-8") for name in FILES if (work / name).exists()}
        differing = changed(current, wanted)
        if not differing:
            print(f"the AUR already has fastbrowse {release.version}")
            return 0
        for name in differing:
            (work / name).write_text(wanted[name], encoding="utf-8")
        _git("add", *differing, cwd=work)
        _git("commit", "--quiet", "-m", f"fastbrowse {release.version}", cwd=work)
        _git("push", "--quiet", "origin", "HEAD:master", cwd=work)
    print(f"the AUR now has fastbrowse {release.version}, changing {', '.join(differing)}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("version", nargs="?", help="the PyPI release to package; the one in pyproject.toml by default")
    parser.add_argument("--root", type=Path, default=REPO, help="the checkout to read, this one by default")
    parser.add_argument("--write", action="store_true", help="set the package to the PyPI release")
    parser.add_argument(
        "--wait", type=float, default=0, metavar="SECONDS", help="how long to wait for PyPI to list and serve it"
    )
    parser.add_argument(
        "--publish", action="store_true", help="push packaging/aur to the AUR, when it already pins this release"
    )
    args = parser.parse_args()
    project = tomllib.loads((args.root / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    version = args.version or project["version"]
    directory = args.root / "packaging" / "aur"
    old = pinned((directory / "PKGBUILD").read_text(encoding="utf-8"))
    if args.publish:
        # Publishing builds on a package that already pins this release and has already been built, so the files
        # are read rather than fetched again: a release is on PyPI by now and its checksums are in the checkout.
        if old.version != version:
            raise SystemExit(f"packaging/aur pins {old.version}, and --publish was asked for {version}")
        return publish(directory, old)
    new = published(version, args.wait)
    if old == new:
        print(f"the AUR package is fastbrowse {new.version} with cdp-use {new.cdp_use} and mcp {new.mcp}")
        return 0
    if not args.write:
        print(f"packaging/aur is fastbrowse {old.version} with cdp-use {old.cdp_use}, and PyPI has:", file=sys.stderr)
        print(f"  fastbrowse {new.version} with cdp-use {new.cdp_use} and mcp {new.mcp}", file=sys.stderr)
        print("run scripts/aur_package.py --write", file=sys.stderr)
        return 1
    for name in FILES:
        path = directory / name
        path.write_text(rewrite(path.read_text(encoding="utf-8"), old, new), encoding="utf-8")
    print(f"the AUR package is now fastbrowse {new.version} with cdp-use {new.cdp_use}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
