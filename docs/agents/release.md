# Releasing

Versions are patch-by-patch unless the maintainer says otherwise, and every one needs a changelog entry:

1. Add the entry under the new version's heading in `CHANGELOG.md` (Keep a Changelog, prose bullets).
2. `uv version <x.y.z>`, then `uv run python scripts/npm_versions.py --write`, which gives the SDK's manifest
   and the Rust workspace the same number. Open a `chore: <x.y.z>` PR. CI fails if the version has no entry or
   npm or Cargo disagrees with it.
3. Before tagging, measure the release commit: `gh workflow run evals.yml --ref <branch-or-tag>`, and for a minor release
   (or a patch tagged with the line `Benchmarks: run`) the campaign `release-<x.y.z>`: every group and suite,
   three repeats, `refresh_competitors` off unless the site needs new competitor rows. Workflow dispatch requires a
   branch or tag, so confirm the resulting run's `head_sha` is the intended release commit before using its results.
   After a later fix, rerun
   only the tasks it affects (`only`, with the same groups and repeats); be generous when the fix changes a
   prompt every task sees. `python3 scripts/release_gate.py --ref <sha> --version <x.y.z>` scores each task on
   the newest campaign run that measured it three times and says whether they pass: no task's pass rate below
   the campaign in `docs/results/release-baseline.json`, and the median run time and mean cost below it.
   The comparison requires valid timing and known cost receipts for every fastbrowse attempt on both sides;
   missing or provisional totals cannot prove an improvement.
   The fixture Evals run must be on the release commit's tree; a squash merge keeps the pull request's tree.
   A rerun's campaign builds the site candidate the same way, each task from its newest run. To build it from the
   finished runs without running anything, dispatch the campaign with `candidate_only` and `fastbrowse_ref` set to
   the newest run's build. The candidate's fixture Evals run must be on that build and the build must be in `main`'s
   history, so build it after the release pull request merges, or dispatch Evals again on the merged commit and rerun
   one task there so the newest run is on it.
4. Before tagging a CLI that requires an extension update, use the [Store workflow](extension-store.md)
   to submit the reviewed package, wait for approval, publish it and verify the Store-installed extension through
   the current CLI. The required manifest version must be published to all users before the CLI publishes.
   Then, once the maintainer says to release, `git tag v<x.y.z> <sha> && git push origin v<x.y.z>`. The tag's workflow
   publishes once the sdist, the wheels and the five binaries build and pass their smoke tests and its `gate`
   job finds those evals passing on the tagged tree. Then point `docs/results/release-baseline.json` at the new
   release's campaign, so the next release is compared with this one. Publish the release commit's fixture rows
   too: download that Evals run's `eval-rows` artifact and run `just evals-publish <dir>/nightly.jsonl`, which
   reads the attempt ledger beside it. Each later fixture run is compared with those rows at the same task
   version.
5. The tag's `aur` job publishes the Arch package and its `aur-sync` job opens the PR that moves
   `packaging/aur` to the release. [aur.md](aur.md) has how the package is built and checked, and what to do
   by hand when either job's secret is missing.

## The release workflow

The [Extension Store workflow](extension-store.md) submits extension changes on `main` for staged review.
Run `just extension-zip` to build the same package locally, with a version and SHA256 receipt. Its manifest
version is independent of the CLI version. CLI tags check the published extension without resubmitting it.
The tag-triggered Store check is a separate receipt. The CLI publishing workflow must check that version
directly before any registry write; a separate Store
workflow's result does not gate another workflow. Publisher authentication and the installed-extension
check are prerequisites, not properties proved by a successful ZIP build.

The header of `.github/workflows/release.yml` lists every registry one tag publishes to and the order it
publishes in. Pull requests and `main` build only the Linux and Windows binaries, leaving the macOS two to the
release. The macOS two, and their Node addons, are signed ad hoc (`codesign --sign -` in
`.github/workflows/binaries.yml`), which is enough for a binary installed through npm. The GitHub release takes
**the changelog entry as its notes**. An entry longer than GitHub's 125,000-character release body is cut at an
item and ends with a link to the whole entry in `CHANGELOG.md` (`scripts/changelog.py --release-notes`).

No registry takes a version back, so finish a release that failed partway by re-running its failed jobs.
Started by hand (`gh workflow run release.yml --ref <branch>`), the workflow stops after the binaries and
publishes nothing: do that after changing the build.

After PyPI the workflow asks fastbrowse.ai to rebuild, since its changelog page reads `CHANGELOG.md` at build
time. `scripts/changelog.py` is what reads the entry, so the repository, the release and the site never tell
three stories about one version. `.github/workflows/site.yml` asks for the same rebuild whenever
`CHANGELOG.md`, `README.md`, `docs/` or `skills/` change on `main`, so the site never waits for a release to
catch up. It needs `SITE_DEPLOY_HOOK` (a Vercel deploy hook for `agent-labs-dev/fastbrowse-site`) in this
repository's secrets; without it the workflows still succeed and warn that the site was not rebuilt.
