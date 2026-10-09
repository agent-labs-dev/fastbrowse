# The Arch package

[packaging/aur](../../packaging/aur) holds the `PKGBUILD` and `.SRCINFO` of the `fastbrowse` AUR package. It
builds two source distributions from PyPI by checksum and installs them into `/opt/fastbrowse`: fastbrowse, and cdp-use at the one exact
version that release requires, which neither Arch nor the AUR packages. Every other dependency is Arch's own
`python-*` package, seen from that prefix. `/usr/bin/fastbrowse` and `/usr/bin/fastbrowse-mcp` link into it.

pacman is told the lower bound of each dependency and no upper one: an upper bound would stop a user's whole
system upgrade on the day Arch passes it. The build check compares the ranges the installed metadata states with what Arch
ships instead, so that day shows as a failed check.

## Checking it

```sh
docker run --rm -v "$PWD:/repo:ro" archlinux:base-devel bash /repo/packaging/aur/build.sh
```

This builds the package in a clean Arch container, compares `.SRCINFO` with what makepkg writes, installs the
package onto its declared dependencies alone, and runs both commands. CI runs it when a pull request changes
the package or the workflows that build it, and the required `check` waits for the result.

## Moving it to a release

The checksums exist only once PyPI holds the release, so the package moves after the tag has published and not
in the pull request that sets the version:

```sh
uv run python scripts/aur_package.py --write   # pkgver, the cdp-use pin and both checksums, in both files
```

The release's `aur` job has by then built that version in the container and kept its `PKGBUILD` and `.SRCINFO`
as the run's `aur` artifact. A change to the package that is not a new version, a rebuild for a new Arch Python
among them, raises `pkgrel` by hand in both files.

## Publishing it

No workflow pushes to the AUR. The maintainer does, with an SSH key registered to the AUR account that owns
`fastbrowse`:

```sh
git clone ssh://aur@aur.archlinux.org/fastbrowse.git ~/fastbrowse-aur
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO ~/fastbrowse-aur/
git -C ~/fastbrowse-aur add PKGBUILD .SRCINFO
git -C ~/fastbrowse-aur commit -m "fastbrowse <x.y.z>"
git -C ~/fastbrowse-aur push origin master
```
