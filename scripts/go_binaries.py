"""Pin a release's fastbrowse binaries in the Go module: the version and each archive's SHA-256, in `go/binaries.go`.

    uv run --no-project python scripts/go_binaries.py 0.9.0 builds/fastbrowse-rust-*/fastbrowse-rust-*.tar.gz

The Go module downloads the archive for its platform from the GitHub release the first time it starts, and refuses
one whose SHA-256 is not the one written here. The release commits the file onto the commit it tags `go/vX.Y.Z`, off
`main`, so a checkout of the repository pins nothing. The file is written as gofmt writes it.

Standard library only, so the release can run it before anything is installed.
"""

import argparse
import hashlib
import re
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
_ARCHIVE = re.compile(r"fastbrowse-rust-(?P<target>[a-z0-9-]+)\.tar\.gz")


def render(source: str, version: str, checksums: dict[str, str]) -> str:
    """`source`, the module's `binaries.go`, with its version and checksums replaced."""
    # gofmt lines the values of a map literal up in one column.
    keys = {target: f'"{target}":' for target in checksums}
    width = max(map(len, keys.values()))
    entries = "".join(f'\t{keys[target]:<{width}} "{checksums[target]}",\n' for target in sorted(checksums))
    text, versions = re.subn(r'(?m)^var releaseVersion = ".*"$', f'var releaseVersion = "{version}"', source)
    text, maps = re.subn(
        r"(?ms)^var releaseChecksums = map\[string\]string\{.*?\}$",
        f"var releaseChecksums = map[string]string{{\n{entries}}}",
        text,
    )
    if versions != 1 or maps != 1:
        raise SystemExit("go/binaries.go has no releaseVersion and releaseChecksums to set")
    return text


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("version")
    parser.add_argument("archives", nargs="+", type=Path)
    parser.add_argument("--file", type=Path, default=REPO / "go" / "binaries.go")
    args = parser.parse_args()
    checksums = {}
    for archive in args.archives:
        named = _ARCHIVE.fullmatch(archive.name)
        if not named:
            raise SystemExit(f"{archive} is not a fastbrowse-rust-<target>.tar.gz")
        checksums[named["target"]] = hashlib.sha256(archive.read_bytes()).hexdigest()
    if set(checksums) != set(TARGETS):
        raise SystemExit(f"the archives are for {sorted(checksums)}, and a release has {list(TARGETS)}")
    args.file.write_text(render(args.file.read_text(encoding="utf-8"), args.version, checksums), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
