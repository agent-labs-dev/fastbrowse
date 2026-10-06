"""Check that the npm packages carry the Python package's version, or with `--write` make them.

    uv run python scripts/npm_versions.py           # exit 1, naming each number that differs
    uv run python scripts/npm_versions.py --write   # after `uv version <x.y.z>`

One tag publishes to PyPI and to npm, and the release takes every number it publishes from `pyproject.toml`.
The SDK's manifest is the one other place a version is written down: its own, and the five platform packages
it depends on. The platform packages' manifests are written at build time by `scripts/npm_platform_package.py`
from the version it is given, so there is nothing of theirs to check here.

Each platform package has to be pinned to the exact version. A range would let an SDK start a binary from a
later release than its own.

Standard library only, so the release can run it before anything is installed.
"""

import argparse
import json
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# The targets `scripts/npm_platform_package.py` assembles a package for.
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")


def differences(version: str, manifest: dict[str, object]) -> list[str]:
    """What in the SDK's manifest is not `version`, one line for each."""
    found = []
    if manifest.get("version") != version:
        found.append(f"fastbrowse is {manifest.get('version')}")
    pins = manifest.get("optionalDependencies")
    pins = pins if isinstance(pins, dict) else {}
    expected = [f"@fastbrowse/{target}" for target in TARGETS]
    for name in expected:
        if name not in pins:
            found.append(f"{name} is missing")
        elif pins[name] != version:
            found.append(f"{name} is {pins[name]}")
    found.extend(f"{name} is not a platform package" for name in pins if name not in expected)
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO, help="the checkout to read, this one by default")
    parser.add_argument("--write", action="store_true", help="set the npm versions to the Python package's")
    args = parser.parse_args()
    version = tomllib.loads((args.root / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    path = args.root / "packages" / "sdk" / "package.json"
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if args.write:
        manifest["version"] = version
        manifest["optionalDependencies"] = {f"@fastbrowse/{target}": version for target in TARGETS}
        path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    found = differences(version, manifest)
    if found:
        print(f"pyproject.toml is {version}, and in {path}:", file=sys.stderr)
        for line in found:
            print(f"  {line}", file=sys.stderr)
        print("run scripts/npm_versions.py --write", file=sys.stderr)
        return 1
    print(f"PyPI and npm are both {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
