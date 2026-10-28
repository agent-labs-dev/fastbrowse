"""Check that the AUR package is the PyPI release, or with `--write` make it.

    uv run python scripts/aur_package.py           # exit 1 when packaging/aur is not this version's release
    uv run python scripts/aur_package.py --write   # after the version is on PyPI

    uv run python scripts/aur_package.py --publish   # after the package has built, put it on the AUR

`packaging/aur/PKGBUILD` builds fastbrowse's source distribution from PyPI by checksum, compiling the Rust core
into the wheel. `--write` writes both files whole from `PKGBUILD` and `SRCINFO` below, so a release from before the
Python core was removed, which needs cdp-use beside it, is refused rather than packaged. The checksum exists only
once PyPI holds the release, so this runs after a tag has published and not in the pull request that sets the
version.

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


PKGBUILD = """\
# Maintainer: Agent Labs <https://github.com/agent-labs-dev/fastbrowse/issues>
#
# Written from the PyPI release by scripts/aur_package.py, which sets pkgver and sha256sums.

pkgname=fastbrowse
pkgver={version}
pkgrel=1
pkgdesc="Browser automation with cited answers, structured output and code-controlled credentials and authorization"
# The wheel carries the Rust core as `fastbrowse._native`, compiled here from the sdist, so the package is one per
# architecture (docs/adr/0004).
arch=('x86_64' 'aarch64')
# makepkg's LTO flags compile ring's C and assembly into bitcode the Rust link does not pull in, which leaves the
# module with undefined ring symbols at import.
options=('!lto')
url="https://github.com/agent-labs-dev/fastbrowse"
license=('MIT')
depends=(
  'python>=3.13'
  'python-h2'
  'python-httpx>=0.28.1'
  'python-pydantic>=2.11'
)
makedepends=('python-build' 'python-installer' 'python-maturin' 'rust')
optdepends=(
  'chromium: a local browser for --local'
  'google-chrome: a local browser for --local'
)
source=("https://files.pythonhosted.org/packages/source/f/fastbrowse/fastbrowse-$pkgver.tar.gz")
sha256sums=('{sha256}')

build() {{
  cd "$srcdir/fastbrowse-$pkgver"
  python -m build --wheel --no-isolation
}}

package() {{
  local prefix=/opt/fastbrowse script
  # A virtual environment is this file and an interpreter beside it. Written by hand, because `python -m venv`
  # records the build directory in the files it makes.
  install -d "$pkgdir$prefix/bin" "$pkgdir/usr/bin"
  ln -s /usr/bin/python "$pkgdir$prefix/bin/python"
  printf 'home = /usr/bin\\ninclude-system-site-packages = true\\n' > "$pkgdir$prefix/pyvenv.cfg"
  python -m installer --destdir="$pkgdir" --prefix="$prefix" "$srcdir/fastbrowse-$pkgver"/dist/*.whl

  for script in fastbrowse fastbrowse-mcp; do
    # The installer names the interpreter that ran it, which would start the system Python and never look in
    # the private prefix.
    sed -i "1s|.*|#!$prefix/bin/python|" "$pkgdir$prefix/bin/$script"
    ln -s "$prefix/bin/$script" "$pkgdir/usr/bin/$script"
  done

  install -Dm644 -t "$pkgdir/usr/share/licenses/$pkgname" "$srcdir/fastbrowse-$pkgver"/{{LICENSE,NOTICE}}
}}
"""
"""The PKGBUILD for one release, given its version and its source distribution's checksum."""

SRCINFO = """\
pkgbase = fastbrowse
\tpkgdesc = Browser automation with cited answers, structured output and code-controlled credentials and authorization
\tpkgver = {version}
\tpkgrel = 1
\turl = https://github.com/agent-labs-dev/fastbrowse
\tarch = x86_64
\tarch = aarch64
\tlicense = MIT
\tmakedepends = python-build
\tmakedepends = python-installer
\tmakedepends = python-maturin
\tmakedepends = rust
\tdepends = python>=3.13
\tdepends = python-h2
\tdepends = python-httpx>=0.28.1
\tdepends = python-pydantic>=2.11
\toptdepends = chromium: a local browser for --local
\toptdepends = google-chrome: a local browser for --local
\toptions = !lto
\tsource = https://files.pythonhosted.org/packages/source/f/fastbrowse/fastbrowse-{version}.tar.gz
\tsha256sums = {sha256}

pkgname = fastbrowse
"""
"""What `makepkg --printsrcinfo` writes for that PKGBUILD, which `packaging/aur/build.sh` checks."""


class Release(NamedTuple):
    """What the package pins: the fastbrowse version, and the checksum of its source distribution."""

    version: str
    sha256: str


def pinned(pkgbuild: str) -> Release:
    """The release a PKGBUILD installs: its version and the checksum of its first source, which is fastbrowse's."""
    version = re.search(r"^pkgver=(\S+)$", pkgbuild, re.MULTILINE)
    sha256 = re.search(r"^sha256sums=\('([0-9a-f]{64})'", pkgbuild, re.MULTILINE)
    if not (version and sha256):
        raise SystemExit("PKGBUILD has no pkgver or sha256sums to read")
    return Release(version[1], sha256[1])


def render(release: Release) -> dict[str, str]:
    """`PKGBUILD` and `.SRCINFO` for `release`, by file name."""
    fields = release._asdict()
    return {"PKGBUILD": PKGBUILD.format(**fields), ".SRCINFO": SRCINFO.format(**fields)}


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
    """The release as PyPI holds it, refused when it still needs cdp-use, which this package does not build."""
    record = _pypi("fastbrowse", version, wait)
    requires = record["info"]["requires_dist"] or []
    if any(re.match(r"cdp-use\b", line) for line in requires):
        raise SystemExit(f"fastbrowse {version} still needs cdp-use, from before the Python core was removed")
    release = Release(version, _source_sum(record))
    if wait:
        # The file the PKGBUILD will fetch, waited for by the address it fetches it from.
        _reachable(source_url("fastbrowse", f"fastbrowse-{version}.tar.gz"), wait)
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
        print(f"the AUR package is fastbrowse {new.version}")
        return 0
    if not args.write:
        print(f"packaging/aur is fastbrowse {old.version}, and PyPI has {new.version}", file=sys.stderr)
        print("run scripts/aur_package.py --write", file=sys.stderr)
        return 1
    for name, text in render(new).items():
        (directory / name).write_text(text, encoding="utf-8")
    print(f"the AUR package is now fastbrowse {new.version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
