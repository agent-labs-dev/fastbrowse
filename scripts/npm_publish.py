"""Publish the six npm packages of a release, and never leave `latest` behind.

    uv run python scripts/npm_publish.py --version 0.9.1 \\
        --platforms "$RUNNER_TEMP/npm" --sdk packages/sdk

`v0.9.1` reached PyPI and the GitHub release on 2026-10-30, and npm kept serving 0.9.0 under `latest`. The
release's `npm` job published `darwin-arm64` and `darwin-x64`, then stopped on `linux-arm64` with
`TLOG_CREATE_ENTRY_ERROR` (409: an equivalent entry already exists in the transparency log). The step was one
`for` loop under `set -e`, so `linux-x64`, `win32-x64` and the SDK never ran, and the SDK is the package the
`latest` dist-tag belongs to.

Two things were missing, and this script is both:

* a target that cannot be published no longer ends the publishing of the others. Every package is attempted,
  and a publish that is refused because the version is already there (or that leaves the version on the
  registry despite reporting an error) counts as done.
* the SDK is published only once all five platform packages are on npm at this version, and the last thing
  this does is read every dist-tag back, once npm's cache has had time to show them. A run that cannot make
  `latest` name the release says so with the exact command that would, instead of ending green.

Standard library only: a release runs this before anything is installed.
"""

import argparse
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

# The five platform packages, and the SDK that depends on them at the same version.
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
SDK = "fastbrowse"
SETTLE_POLL = 10  # seconds between reads of `latest` while the registry cache catches up

# The registry refuses a second probe of the same bytes with this, and will refuse every retry of the same
# tarball the same way: the transparency-log entry is keyed on the content, not on the attempt.
TLOG_CONFLICT = "TLOG_CREATE_ENTRY_ERROR"


@dataclass(frozen=True)
class Attempt:
    """One `npm publish`: whether it worked, and what npm said when it did not."""

    ok: bool
    output: str

    @property
    def collision(self) -> bool:
        return TLOG_CONFLICT in self.output

    def summary(self) -> str:
        """The line of npm's output that says what happened, without its exit status or its silence."""
        lines = [line.strip() for line in self.output.splitlines() if line.strip()]
        said = [line for line in lines if "error" in line.lower()] or lines
        return said[0] if said else "npm said nothing"


def run(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(arguments, capture_output=True, text=True, check=False)


def published(package: str, version: str) -> bool:
    """Whether npm already holds this exact version of this package."""
    return run("npm", "view", f"{package}@{version}", "version").returncode == 0


def latest(package: str) -> str:
    """The version `npm install <package>` resolves to."""
    result = run("npm", "view", package, "dist-tags.latest")
    return result.stdout.strip() if result.returncode == 0 else ""


def publish(path: Path, version: str) -> Attempt:
    """Publish one directory, then believe the registry rather than npm's exit code."""
    # npm reads a relative `owner/name` path, such as the workflow's `packages/sdk`, as a GitHub repository and
    # tries to clone it (0.10.0's SDK failed with git's exit code 128), so it is given the absolute path.
    result = run("npm", "publish", str(path.resolve()), "--access", "public", "--tag", "latest")
    output = f"{result.stdout}\n{result.stderr}".strip()
    return Attempt(result.returncode == 0, output)


def publish_one(package: str, path: Path, version: str) -> tuple[bool, str]:
    """Publish `path` as `package`, skipping and retrying the cases npm makes safe to retry.

    Returns whether the version is on npm afterwards, and a line to print about it.
    """
    if published(package, version):
        return True, f"{package}@{version} is already published"
    attempt = publish(path, version)
    if attempt.ok or published(package, version):
        # npm can report an error for a publish the registry accepted; the version is what counts.
        return True, f"published {package}@{version}"
    if attempt.collision:
        # A second attempt sends the same tarball, which can only produce the same entry.
        return False, (
            f"{package}@{version} is refused by the transparency log: an entry for these exact bytes already "
            f"exists from an earlier attempt whose registry write did not land. Retrying this tarball cannot "
            f"succeed. Publish it with `npm publish <tarball> --access public --tag latest --provenance=false` "
            f"by hand after review, or ship it in the next tag."
        )
    # A network or 5xx failure is worth one more try.
    retry = publish(path, version)
    if retry.ok or published(package, version):
        return True, f"published {package}@{version} on the second attempt"
    return False, f"{package}@{version} could not be published: {retry.summary() or attempt.summary()}"


def differences(version: str) -> list[tuple[str, str]]:
    """Every package whose `latest` dist-tag does not name this release."""
    found = []
    for package in (SDK, *(f"@fastbrowse/{target}" for target in TARGETS)):
        named = latest(package)
        if named != version:
            found.append((package, named))
    return found


def settled_differences(version: str, settle: float) -> list[tuple[str, str]]:
    """The packages whose `latest` still does not name this release once the registry has had `settle` seconds.

    npm serves package documents from a cache, so a read straight after an accepted publish can show the old
    `latest`: 0.10.0's SDK read back 0.9.1 at 00:31:45 and became visible at 00:32:59. Drift is what remains
    after that window, not what the first read shows.
    """
    deadline = time.monotonic() + settle
    while (drifted := differences(version)) and (remaining := deadline - time.monotonic()) > 0:
        time.sleep(min(SETTLE_POLL, remaining))
    return drifted


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True, help="The tag's version, which every package carries.")
    parser.add_argument(
        "--platforms", type=Path, required=True, help="The directory holding one built package per target."
    )
    parser.add_argument("--sdk", type=Path, required=True, help="The SDK package directory.")
    parser.add_argument(
        "--settle",
        type=float,
        default=300,
        help="Seconds to wait for npm's cached `latest` to show the release before calling it drift.",
    )
    arguments = parser.parse_args(argv)
    version = arguments.version

    failed = []
    for target in TARGETS:
        package = f"@fastbrowse/{target}"
        ok, line = publish_one(package, arguments.platforms / target, version)
        print(line, flush=True)
        if not ok:
            failed.append(package)

    if failed:
        # The SDK points at all five at this exact version, so publishing it now would install nothing.
        print(
            f"Not publishing {SDK}@{version}: it depends on {len(failed)} platform package(s) that are not on "
            f"npm: {', '.join(failed)}.",
            file=sys.stderr,
        )
        return 1

    ok, line = publish_one(SDK, arguments.sdk, version)
    print(line, flush=True)
    if not ok:
        print(
            f"{SDK}@{version} is not on npm, so `npm install {SDK}` still serves the previous release.",
            file=sys.stderr,
        )
        return 1

    drifted = settled_differences(version, arguments.settle)
    if drifted:
        print(f"npm publishes {version} but `latest` does not name it:", file=sys.stderr)
        for package, named in drifted:
            print(f"  {package}: latest is {named or 'unreadable'}", file=sys.stderr)
            print(f"    npm dist-tag add {package}@{version} latest", file=sys.stderr)
        return 1

    print(f"npm serves {version} as `latest` for {len(TARGETS) + 1} packages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
