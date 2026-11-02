# Releasing

Versions are patch-by-patch unless the maintainer says otherwise, and every one needs a changelog entry:

1. Add the entry under the new version's heading in `CHANGELOG.md` (Keep a Changelog, prose bullets).
2. `uv version <x.y.z>`, then `uv run python scripts/npm_versions.py --write`, which gives the SDK's manifest
   and the Rust workspace the same number. Open a `chore: <x.y.z>` PR. CI fails if the version has no entry or
   npm or Cargo disagrees with it.
3. Once the maintainer says to release, `git tag v<x.y.z> <sha> && git push origin v<x.y.z>`. The tag's workflow
   publishes once the sdist, the wheels and the five binaries build and pass their smoke tests. The fixture
   suites (`.github/workflows/evals.yml`) run nightly and on demand, and do not gate a release.
4. The tag's `aur` job publishes the Arch package and its `aur-sync` job opens the PR that moves
   `packaging/aur` to the release. [aur.md](aur.md) has how the package is built and checked, and what to do
   by hand when either job's secret is missing.

## The release workflow

The header of `.github/workflows/release.yml` lists every registry one tag publishes to and the order it
publishes in. Pull requests and `main` build only the Linux and Windows binaries, leaving the macOS two to the
release. The macOS two, and their Node addons, are signed ad hoc (`codesign --sign -` in
`.github/workflows/binaries.yml`), which is enough for a binary installed through npm. The GitHub release takes
**the changelog entry as its notes**.

No registry takes a version back, so finish a release that failed partway by re-running its failed jobs.
Started by hand (`gh workflow run release.yml --ref <branch>`), the workflow stops after the binaries and
publishes nothing: do that after changing the build.

After PyPI the workflow asks fastbrowse.ai to rebuild, since its changelog page reads `CHANGELOG.md` at build
time. `scripts/changelog.py` is what reads the entry, so the repository, the release and the site never tell
three stories about one version. `.github/workflows/site.yml` asks for the same rebuild whenever
`CHANGELOG.md`, `README.md`, `docs/` or `skills/` change on `main`, so the site never waits for a release to
catch up. It needs `SITE_DEPLOY_HOOK` (a Vercel deploy hook for `agent-labs-dev/fastbrowse-site`) in this
repository's secrets; without it the workflows still succeed and warn that the site was not rebuilt.
