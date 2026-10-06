"""Assemble one npm platform package, `@fastbrowse/<os>-<arch>`, from one frozen build.

    python scripts/npm_platform_package.py dist/fastbrowse --target darwin-arm64 --version 0.6.0 --out pkg
    python scripts/npm_platform_package.py fastbrowse-linux-x64.tar.gz --target linux-x64 --version 0.6.0 --out pkg

The build is PyInstaller's output directory, or the archive of it that the binaries workflow uploads. The
directory written to `--out` is what `npm publish` takes: a manifest, the licence files, a README, and the build
under `fastbrowse/`.

The five manifests are written here and exist nowhere else, so the version has one place to come from, the
caller. It is refused unless it is the version of the fastbrowse inside the build, and a package therefore
cannot be published under a number its executable would not report.

Standard library only: it runs on a build runner with nothing installed.
"""

import argparse
import json
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


def executable_name(target: str) -> str:
    return "fastbrowse.exe" if target.startswith("win32-") else "fastbrowse"


def manifest(target: str, version: str) -> dict[str, object]:
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
        "files": ["fastbrowse", "NOTICE"],
        # Yarn's Plug'n'Play keeps a package zipped unless told to unpack it, and a zipped file cannot be run.
        "preferUnplugged": True,
        # A scoped package is private unless its first publish says otherwise.
        "publishConfig": {"access": "public"},
    }


def _check(tree: Path, target: str, version: str) -> None:
    executable = tree / executable_name(target)
    if not executable.is_file() or not (tree / "_internal").is_dir():
        raise Refused(f"{tree} is not a {target} build: it needs {executable.name} and _internal/ beside it")
    held = sorted(path.name.removesuffix(".dist-info") for path in (tree / "_internal").glob("fastbrowse-*.dist-info"))
    if held != [f"fastbrowse-{version}"]:
        raise Refused(f"the build holds {', '.join(held) or 'no fastbrowse metadata'}, and was to be version {version}")


def assemble(frozen: Path, target: str, version: str, out: Path) -> None:
    """Write the package for `target` to `out` from the build at `frozen`, a directory or a `.tar.gz` of one."""
    if target not in TARGETS:
        raise Refused(f"{target} is not a target: {', '.join(TARGETS)}")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        raise Refused(f"{out} is in the way: it has to be an empty directory or not exist")
    with tempfile.TemporaryDirectory() as unpacked:
        if frozen.is_dir():
            tree = frozen
        elif frozen.is_file():
            with tarfile.open(frozen) as archive:
                archive.extractall(unpacked, filter="data")
            tree = Path(unpacked) / "fastbrowse"
        else:
            raise Refused(f"{frozen} is neither a build directory nor an archive of one")
        _check(tree, target, version)
        out.mkdir(parents=True, exist_ok=True)
        # npm packs no symbolic link, and PyInstaller links a library it would otherwise carry twice, so each
        # link is copied as the file it points to. npm packs no `._<file>` either, which is how macOS tar
        # archives a file's extended attributes and what they unpack to here.
        shutil.copytree(tree, out / "fastbrowse", symlinks=False, ignore=shutil.ignore_patterns("._*"))
    if not target.startswith("win32-"):
        # npm keeps the mode a file is packed with, and this is the file the SDK executes.
        (out / "fastbrowse" / executable_name(target)).chmod(0o755)
    (out / "package.json").write_text(json.dumps(manifest(target, version), indent=2) + "\n", encoding="utf-8")
    system, cpu = target.split("-")
    (out / "README.md").write_text(README.format(target=target, system=SYSTEMS[system], cpu=cpu), encoding="utf-8")
    for name in ("LICENSE", "NOTICE"):
        shutil.copyfile(REPO / name, out / name)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("frozen", type=Path, help="the build directory, or a .tar.gz that unpacks to fastbrowse/")
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--version", required=True, help="the version to publish, which the build must hold")
    parser.add_argument("--out", required=True, type=Path, help="the package directory to write")
    args = parser.parse_args()
    try:
        assemble(args.frozen, args.target, args.version, args.out)
    except Refused as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 1
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
