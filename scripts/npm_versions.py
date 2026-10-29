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
`--version` and reports to a `serve` client, and the platform packages carry that binary. The crates published to
crates.io pin each other to that version exactly in `[workspace.dependencies]`. `--write` moves all of them and the
workspace's own packages in `Cargo.lock` with it, so `cargo build --locked` still passes.

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
# The Java client's own version in `java/pom.xml`: the first `<version>`, right after its `<artifactId>`.
_POM_VERSION = re.compile(r"(<artifactId>fastbrowse</artifactId>\s*<version>)([^<]*)(</version>)")
# The .NET client's version in its project file.
_CSPROJ_VERSION = re.compile(r"(<Version>)([^<]*)(</Version>)")
_CSPROJ = Path("dotnet") / "src" / "Fastbrowse" / "Fastbrowse.csproj"
# One workspace crate as another depends on it: `fastbrowse-core = { path = "...", version = "=0.8.3" }`.
_PIN = re.compile(r'(?m)^(fastbrowse[a-z-]* = \{ path = "[^"]+", version = "=)[^"]*(" \})$')


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


def pins(cargo: str) -> dict[str, object]:
    """Each crate's version requirement in `[workspace.dependencies]`, such as `=0.8.3`."""
    dependencies = tomllib.loads(cargo).get("workspace", {}).get("dependencies", {})
    return {
        name: entry.get("version")
        for name, entry in dependencies.items()
        if name.startswith("fastbrowse") and isinstance(entry, dict) and "path" in entry
    }


def pom_version(root: Path) -> str | None:
    """The Java client's version, or None in a checkout without one."""
    pom = root / "java" / "pom.xml"
    if not pom.exists():
        return None
    found = _POM_VERSION.search(pom.read_text(encoding="utf-8"))
    return found[2] if found else ""


def write_pom(root: Path, version: str) -> None:
    pom = root / "java" / "pom.xml"
    if pom.exists():
        text, count = _POM_VERSION.subn(rf"\g<1>{version}\g<3>", pom.read_text(encoding="utf-8"), count=1)
        if count != 1:
            raise SystemExit(f"{pom} has no project version to set")
        pom.write_text(text, encoding="utf-8")


def csproj_version(root: Path) -> str | None:
    """The .NET client's version, or None in a checkout without one."""
    project = root / _CSPROJ
    if not project.exists():
        return None
    found = _CSPROJ_VERSION.search(project.read_text(encoding="utf-8"))
    return found[2] if found else ""


def write_csproj(root: Path, version: str) -> None:
    project = root / _CSPROJ
    if project.exists():
        text, count = _CSPROJ_VERSION.subn(rf"\g<1>{version}\g<3>", project.read_text(encoding="utf-8"), count=1)
        if count != 1:
            raise SystemExit(f"{project} has no <Version> to set")
        project.write_text(text, encoding="utf-8")


def write_workspace(root: Path, version: str) -> None:
    """Set the Rust workspace's version, and its own packages' entries in `Cargo.lock`, to `version`."""
    cargo = root / "Cargo.toml"
    before = cargo.read_text(encoding="utf-8")
    text, count = _WORKSPACE_VERSION.subn(rf"\g<1>{version}\g<2>", before)
    if count != 1:
        raise SystemExit(f"{cargo} has no [workspace.package] version to set")
    text, count = _PIN.subn(rf"\g<1>{version}\g<2>", text)
    if count != len(pins(before)):
        raise SystemExit(
            f"{cargo}: {len(pins(before)) - count} [workspace.dependencies] pins are not in the form to set"
        )
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
        write_pom(args.root, version)
        write_csproj(args.root, version)
    found = [f"{path}: {line}" for line in differences(version, manifest)]
    rust = workspace_version(cargo.read_text(encoding="utf-8"))
    if rust != version:
        found.append(f"{cargo}: the workspace is {rust}")
    found.extend(
        f"{cargo}: {name} is pinned to {pinned}"
        for name, pinned in pins(cargo.read_text(encoding="utf-8")).items()
        if pinned != f"={version}"
    )
    java = pom_version(args.root)
    if java is not None and java != version:
        found.append(f"{args.root / 'java' / 'pom.xml'}: the Java client is {java}")
    dotnet = csproj_version(args.root)
    if dotnet is not None and dotnet != version:
        found.append(f"{args.root / _CSPROJ}: the .NET client is {dotnet}")
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
