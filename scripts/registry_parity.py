"""Check that the six npm packages and PyPI name the same latest version, or fail.

    uv run python scripts/registry_parity.py                    # PyPI's latest is the version to match
    uv run python scripts/registry_parity.py --version 0.9.1    # the release's version is

One tag publishes one version to PyPI and to npm, six packages of it, and each publish is guarded so a run
that stopped partway can be run again. What nothing checked is the result: 0.9.1 reached PyPI and the GitHub
release on 2026-10-30, npm published two of the five platform packages, and `fastbrowse` stayed at 0.9.0
under `latest` for four days. Every JavaScript install in that window got the older code and the release run
said nothing, because the job that failed had already failed.

This reads both registries and exits 1 when a dist-tag disagrees with the version, so the release run that
left the registries apart fails on its own, and the hourly schedule says so while no release is running.

Standard library only, so a release can run it before anything is installed.
"""

import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

# The five platform packages, and the SDK that depends on them at the same version.
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
NPM_PACKAGES = ("fastbrowse",) + tuple(f"@fastbrowse/{target}" for target in TARGETS)

PYPI = "https://pypi.org/pypi/fastbrowse/json"
NPM = "https://registry.npmjs.org"
TIMEOUT_SECONDS = 30
USER_AGENT = "fastbrowse registry parity (github.com/agent-labs-dev/fastbrowse)"


def _get_json(url: str) -> object:
    """The JSON a registry serves, or the error that stopped us reading it."""
    request = urllib.request.Request(
        url, headers={"Accept": "application/json", "User-Agent": USER_AGENT}
    )
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        return json.load(response)


def pypi_latest(pypi_url: str) -> str:
    document = _get_json(pypi_url)
    version = document["info"]["version"] if isinstance(document, dict) else None
    if not isinstance(version, str) or not version:
        raise ValueError(f"{pypi_url} names no version")
    return version


def npm_latest(npm_url: str, package: str) -> str:
    """The version `npm install <package>` resolves to: the `latest` dist-tag."""
    # A scoped name is one path segment on this endpoint, so only its slash is escaped.
    name = urllib.parse.quote(package, safe="@")
    document = _get_json(f"{npm_url}/-/package/{name}/dist-tags")
    version = document.get("latest") if isinstance(document, dict) else None
    if not isinstance(version, str) or not version:
        raise ValueError(f"{package} has no latest dist-tag")
    return version


def differences(
    version: str | None, *, pypi_url: str = PYPI, npm_url: str = NPM
) -> list[str]:
    """Every place the registries disagree, one line each. Empty when they agree."""
    try:
        published = pypi_latest(pypi_url)
    except (OSError, ValueError, KeyError, TypeError) as error:
        # A guard that cannot read a registry has to say so; passing would hide the drift it exists for.
        return [f"PyPI could not be read at {pypi_url}: {error}"]
    expected = version or published
    found = []
    if published != expected:
        found.append(f"PyPI's latest is {published}, not {expected}")
    for package in NPM_PACKAGES:
        try:
            latest = npm_latest(npm_url, package)
        except (OSError, ValueError, KeyError, TypeError) as error:
            found.append(f"{package} could not be read on npm: {error}")
            continue
        if latest != expected:
            found.append(f"{package}'s npm latest is {latest}, not {expected}")
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--version",
        default=None,
        help="The version every registry has to carry. PyPI's latest when left out, or empty.",
    )
    # For the tests, and for reading a registry through a mirror.
    parser.add_argument("--pypi-url", default=PYPI)
    parser.add_argument("--npm-url", default=NPM)
    arguments = parser.parse_args(argv)
    found = differences(
        arguments.version or None,
        pypi_url=arguments.pypi_url,
        npm_url=arguments.npm_url,
    )
    if found:
        print("The registries disagree:", file=sys.stderr)
        for line in found:
            print(f"  {line}", file=sys.stderr)
        print(
            "Publish the missing version under the `latest` dist-tag, by re-running the release's failed"
            " jobs, which pass over what is already published.",
            file=sys.stderr,
        )
        return 1
    print(
        f"PyPI and {len(NPM_PACKAGES)} npm packages all name {arguments.version or pypi_latest(arguments.pypi_url)}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
