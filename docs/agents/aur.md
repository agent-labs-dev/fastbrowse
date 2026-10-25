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

The AUR workflow also runs weekly. It builds the committed package, since Arch moves between our changes, and
runs `scripts/aur_package.py` without `--write`, which fails while `packaging/aur` is older than the PyPI
release in `pyproject.toml`. A release that skipped this step shows there.

## Publishing it

The release publishes it. Once the Arch build in the `aur` job has passed, a release that was given a version
runs `scripts/aur_package.py <version> --publish`, which clones `ssh://aur@aur.archlinux.org/fastbrowse.git`,
copies `PKGBUILD` and `.SRCINFO`, and commits only the files whose bytes differ, so republishing a version is
never a new AUR revision that changes nothing.

Two repository secrets make that automatic. Without either, a release still builds and smoke-tests the package and
keeps its `aur` artifact, and the run says in a warning what did not happen:

- `AUR_SSH_PRIVATE_KEY`: a private key whose public half is registered to the AUR account that owns
  `fastbrowse`. This is the one step of a release that cannot run without a secret, because the AUR authenticates
  the maintainer and not the repository.
- `AUR_SYNC_TOKEN`: a token that may push and open pull requests here. The `aur-sync` job regenerates
  `packaging/aur` from the PyPI release and opens the PR that puts those bytes on `main`; the token is needed
  because a PR opened with the run's own token starts no checks, and the required `check` could never report on
  it. When it is unset the job says so and `main` keeps the previous package.

To publish by hand, with the same key:

```sh
git clone ssh://aur@aur.archlinux.org/fastbrowse.git ~/fastbrowse-aur
cp packaging/aur/PKGBUILD packaging/aur/.SRCINFO ~/fastbrowse-aur/
git -C ~/fastbrowse-aur add PKGBUILD .SRCINFO
git -C ~/fastbrowse-aur commit -m "fastbrowse <x.y.z>"
git -C ~/fastbrowse-aur push origin master
```

### Recovering a release that published nothing

A release can stop after PyPI accepted the upload but before its file host served the file; `--wait 300` now waits
for the two source distributions themselves, so this should not recur. If the `aur` job still fails, its
`Move the package to the release` step says which address did not answer, and re-running the failed jobs from the
Actions page publishes again: `--publish` reads what the AUR already holds before committing, so a rerun after a
half-finished push commits only what is still missing.
