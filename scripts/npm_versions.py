"""Check that the npm packages and the Rust workspace carry the Python package's version, or with `--write` make them.

    uv run python scripts/npm_versions.py           # exit 1, naming each number that differs
    uv run python scripts/npm_versions.py --write   # after `uv version <x.y.z>`

One tag publishes to PyPI and to npm, and the release takes every number it publishes from `pyproject.toml`.
The SDK's manifest is one other place a version is written down: its own, and the five platform packages
it depends on. The platform packages' manifests are written at build time by `scripts/npm_platform_package.py`
from the version it is given, so there is nothing of theirs to check here.

Each platform package has to be pinned to the exact version. A range would let an SDK start a binary from a
later release than its own.

The Rust workspace is the other: `Cargo.toml`'s `[workspace.package]` version is what the Rust binary prints for
`--version` and reports to a `serve` client, and the platform packages carry that binary. `--write` moves it and
the workspace's own packages in `Cargo.lock` with it, so `cargo build --locked` still passes.

Standard library only, so the release can run it before anything is installed.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# The targets `scripts/npm_platform_package.py` assembles a package for.
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")

_WORKSPACE_VERSION = re.compile(r'(?m)^(\[workspace\.package\]\n(?:[^\[\n].*\n)*?version = ")[^"]*(")')


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


def workspace_version(cargo: str) -> object:
    """`[workspace.package] version` in a `Cargo.toml`, or None when it has none."""
    package = tomllib.loads(cargo).get("workspace", {}).get("package", {})
    return package.get("version")


def write_workspace(root: Path, version: str) -> None:
    """Set the Rust workspace's version, and its own packages' entries in `Cargo.lock`, to `version`."""
    cargo = root / "Cargo.toml"
    before = cargo.read_text(encoding="utf-8")
    text, count = _WORKSPACE_VERSION.subn(rf"\g<1>{version}\g<2>", before)
    if count != 1:
        raise SystemExit(f"{cargo} has no [workspace.package] version to set")
    cargo.write_text(text, encoding="utf-8")
    lock = root / "Cargo.lock"
    if not lock.exists():
        return
    # A package with no `source` is a path dependency. Only those at the old workspace version inherit it: a member
    # that sets its own version, as the probe does, keeps it.
    old = re.escape(str(workspace_version(before)))
    blocks = lock.read_text(encoding="utf-8").split("\n[[package]]\n")
    for i, block in enumerate(blocks):
        if block.startswith("name = ") and "\nsource = " not in block:
            blocks[i] = re.sub(rf'^(name = "[^"]+"\nversion = "){old}(")', rf"\g<1>{version}\g<2>", block)
    lock.write_text("\n[[package]]\n".join(blocks), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO, help="the checkout to read, this one by default")
    parser.add_argument("--write", action="store_true", help="set the npm and Rust versions to the Python package's")
    args = parser.parse_args()
    version = tomllib.loads((args.root / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    path = args.root / "packages" / "sdk" / "package.json"
    manifest = json.loads(path.read_text(encoding="utf-8"))
    cargo = args.root / "Cargo.toml"
    if args.write:
        manifest["version"] = version
        manifest["optionalDependencies"] = {f"@fastbrowse/{target}": version for target in TARGETS}
        path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        write_workspace(args.root, version)
    found = [f"{path}: {line}" for line in differences(version, manifest)]
    rust = workspace_version(cargo.read_text(encoding="utf-8"))
    if rust != version:
        found.append(f"{cargo}: the workspace is {rust}")
    if found:
        print(f"pyproject.toml is {version}, and in", file=sys.stderr)
        for line in found:
            print(f"  {line}", file=sys.stderr)
        print("run scripts/npm_versions.py --write", file=sys.stderr)
        return 1
    print(f"PyPI, npm and the Rust workspace are all {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
