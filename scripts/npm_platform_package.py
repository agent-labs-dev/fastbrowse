"""Assemble one npm platform package, `@fastbrowse/<os>-<arch>`, from one Rust binary.

    python scripts/npm_platform_package.py target/release-small/fastbrowse --target darwin-arm64 \\
        --version 0.6.0 --out pkg [--addon fastbrowse.node]
    python scripts/npm_platform_package.py fastbrowse-rust-linux-x64.tar.gz --target linux-x64 --version 0.6.0 --out pkg

The build is the single `fastbrowse` executable of `crates/fastbrowse-cli`, or the archive the binaries workflow
uploads, holding it under `fastbrowse/`. The package carries it twice, as `fastbrowse` and as `fastbrowse-mcp`,
because the binary serves MCP when it is started under that name and npm packs no symbolic link. `--addon` adds
the Node addon of `crates/fastbrowse-node` as `fastbrowse.node`, which the SDK loads for `inProcess`.

The directory written to `--out` is what `npm publish` takes: a manifest, the licence files, a README, and the
build under `fastbrowse/`.

The five manifests are written here and exist nowhere else, so the version has one place to come from, the
caller. It is refused unless it is the version of the fastbrowse inside the build, and a package therefore
cannot be published under a number its executable would not report.

Standard library only: it runs on a build runner with nothing installed.
"""

import argparse
import json
import re
import shutil
import sys
import tarfile
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# The targets the binaries workflow builds. Each is npm's `os` and `cpu`, which are Node's `process.platform`
# and `process.arch`, so the SDK names its own package from those two.
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")

SYSTEMS = {"darwin": "macOS", "linux": "Linux", "win32": "Windows"}

README = """# @fastbrowse/{target}

The fastbrowse binary for {system} on {cpu}. Install [`fastbrowse`](https://www.npmjs.com/package/fastbrowse)
and not this package: it depends on the one that fits your machine, and starts the binary in it.
"""


class Refused(Exception):
    """An input no package can be assembled from, said so that the caller can fix it."""


def executable_name(target: str, name: str = "fastbrowse") -> str:
    return f"{name}.exe" if target.startswith("win32-") else name


def manifest(target: str, version: str, *, addon: bool = False) -> dict[str, object]:
    """The package's `package.json`. It has no `scripts`: an install runs nothing."""
    system, cpu = target.split("-")
    return {
        "name": f"@fastbrowse/{target}",
        "version": version,
        "description": f"The fastbrowse binary for {SYSTEMS[system]} on {cpu}.",
        "license": "MIT",
        # npm checks a provenance attestation against this address.
        "repository": {"type": "git", "url": "git+https://github.com/agent-labs-dev/fastbrowse.git"},
        "os": [system],
        "cpu": [cpu],
        # The Linux builds link glibc. With this, npm leaves the package out on a musl system such as Alpine
        # and the SDK can say why there is no binary, where the binary itself would fail to load.
        **({"libc": ["glibc"]} if system == "linux" else {}),
        "files": ["fastbrowse", "NOTICE", *(["fastbrowse.node"] if addon else [])],
        # Yarn's Plug'n'Play keeps a package zipped unless told to unpack it, and a zipped file cannot be run.
        "preferUnplugged": True,
        # A scoped package is private unless its first publish says otherwise.
        "publishConfig": {"access": "public"},
    }


def _check(executable: Path, target: str, version: str) -> None:
    if not executable.is_file():
        raise Refused(f"{executable} is not a {target} build: it needs to be the fastbrowse executable")
    # The binary cannot be run here when it was built for another target, and the version it reports is a
    # string literal inside it. A digit or a dot on either side would make it part of another number.
    held = re.compile(rb"(?<![0-9.])" + re.escape(version.encode()) + rb"(?![0-9.])")
    if not held.search(executable.read_bytes()):
        raise Refused(f"{executable} holds no version {version}, and was to be that version")


def assemble(build: Path, target: str, version: str, out: Path, *, addon: Path | None = None) -> None:
    """Write the package for `target` to `out` from `build`, the executable or a `.tar.gz` holding it under
    `fastbrowse/`."""
    if target not in TARGETS:
        raise Refused(f"{target} is not a target: {', '.join(TARGETS)}")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        raise Refused(f"{out} is in the way: it has to be an empty directory or not exist")
    if addon is not None and not addon.is_file():
        raise Refused(f"{addon} is not a Node addon")
    with tempfile.TemporaryDirectory() as unpacked:
        executable = build
        if build.is_file() and tarfile.is_tarfile(build):
            with tarfile.open(build) as archive:
                archive.extractall(unpacked, filter="data")
            executable = Path(unpacked) / "fastbrowse" / executable_name(target)
        _check(executable, target, version)
        out.mkdir(parents=True, exist_ok=True)
        (out / "fastbrowse").mkdir()
        for name in ("fastbrowse", "fastbrowse-mcp"):
            shutil.copyfile(executable, out / "fastbrowse" / executable_name(target, name))
    if not target.startswith("win32-"):
        # npm keeps the mode a file is packed with, and these are the files the SDK and an MCP client execute.
        for name in ("fastbrowse", "fastbrowse-mcp"):
            (out / "fastbrowse" / executable_name(target, name)).chmod(0o755)
    if addon is not None:
        shutil.copyfile(addon, out / "fastbrowse.node")
    package = manifest(target, version, addon=addon is not None)
    (out / "package.json").write_text(json.dumps(package, indent=2) + "\n", encoding="utf-8")
    system, cpu = target.split("-")
    (out / "README.md").write_text(README.format(target=target, system=SYSTEMS[system], cpu=cpu), encoding="utf-8")
    for name in ("LICENSE", "NOTICE"):
        shutil.copyfile(REPO / name, out / name)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("build", type=Path, help="the fastbrowse executable, or a .tar.gz of fastbrowse/")
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--version", required=True, help="the version to publish, which the build must hold")
    parser.add_argument("--out", required=True, type=Path, help="the package directory to write")
    parser.add_argument("--addon", type=Path, help="the Node addon to carry as fastbrowse.node")
    args = parser.parse_args()
    try:
        assemble(args.build, args.target, args.version, args.out, addon=args.addon)
    except Refused as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 1
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
