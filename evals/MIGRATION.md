# Moving the workflows onto `evals/`

The campaign and official workflows still check out `agent-labs-dev/fastbrowse-evals` and run `parallax browser`.
This checklist moves them onto this project. Apply it to `main` once 0.10.0 is tagged, in one pull request,
after rebasing this branch with `git rebase --onto origin/main origin/cb/recipes`.

## What changes for every step

| Before | After |
| --- | --- |
| `parallax browser <command>` | `fastbrowse-evals browser <command>`, same flags |
| `cd parallax` | `cd harness/evals` |
| `parallax_sha` input, `PARALLAX_SHA` variable | `harness_sha` input, optional, defaulting to `github.sha` |
| `EVALS_DEPLOY_KEY` | not needed: the harness is this repository |
| harness `Parallax at <sha>` | harness `fastbrowse-evals at <sha>` |

The commands are `internal`, `official`, `catalog`, `track-ledger`, `track-official`, `official-rows` and
`publish-candidate`. `PARALLAX_JUDGE_*`, `PARALLAX_LANGFUSE_PROJECT` and the official judge variables keep their
names, since Parallax 2.0 reads them.

## `.github/workflows/benchmark-campaign.yml`

- [ ] Header comments (lines 11 to 17, 35): the campaign code is `evals/` in this repository at `harness_sha`.
  Drop the deploy key paragraph and `EVALS_DEPLOY_KEY` from the secrets list.
- [ ] `on.workflow_dispatch.inputs.parallax_sha` becomes:

  ```yaml
  harness_sha:
    description: Full commit SHA of this repository whose evals/ runs the campaign; empty runs this workflow's commit
    required: false
    type: string
  ```

- [ ] Resolve the pins: read `HARNESS_SHA: ${{ inputs.harness_sha || github.event.client_payload.harness_sha || github.sha }}`,
  keep the 40 character check under the new name, and write `harness_sha=$HARNESS_SHA` to the output. Rename
  the `parallax_sha` job output to `harness_sha`.
- [ ] Preflight: delete `EVALS_DEPLOY_KEY` from `env` and the `need EVALS_DEPLOY_KEY` line.
- [ ] Run and stage jobs: replace "Check out the pinned parallax" with

  ```yaml
  - name: Check out the harness
    uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
    with:
      ref: ${{ needs.resolve.outputs.harness_sha }}
      path: harness
      persist-credentials: false
  ```

- [ ] Install step, renamed "Install the harness and fastbrowse":

  ```sh
  cd harness/evals
  # The lock pins fastbrowse to this checkout; the measured build replaces it. Every later command runs with
  # --no-sync, since a syncing `uv run` would put the harness's own fastbrowse back.
  uv sync --locked --no-install-package fastbrowse
  uv pip install "fastbrowse @ git+file://$GITHUB_WORKSPACE/fastbrowse@$FASTBROWSE_SHA"
  uv run --no-sync python -c "import fastbrowse, fastbrowse_evals.live"
  ```

  `--extra browser --extra track` go: the project has no extras.
- [ ] Run, track, catalog and candidate steps: `cd harness/evals` and `uv run --no-sync fastbrowse-evals browser ...`
  for `internal`, `track-ledger`, `catalog` and `publish-candidate`. No flag changes.
- [ ] Run facts: `PARALLAX_SHA: ${{ needs.resolve.outputs.parallax_sha }}` becomes `HARNESS_SHA: ${{ needs.resolve.outputs.harness_sha }}`,
  and `--arg harness "Parallax at ${PARALLAX_SHA:0:7}"` becomes `--arg harness "fastbrowse-evals at ${HARNESS_SHA:0:7}"`.

## `.github/workflows/official-benchmarks.yml`

- [ ] Header comments (lines 3 to 47): `fastbrowse-evals browser official` and `official-rows`, the harness is
  `evals/` at `harness_sha`, and drop `EVALS_DEPLOY_KEY` from the secrets list and the fastbrowse-evals#361 note.
- [ ] Same input, pin, preflight, checkout and install edits as the campaign. The import check is
  `import fastbrowse, fastbrowse_evals.publication`.
- [ ] Mind2Web fetch step: `cd harness/evals`, and

  ```python
  from fastbrowse.evals.sources import SOURCES
  from fastbrowse_evals.datasets import fetch
  ```

- [ ] `official`, `track-official` and `official-rows` steps: `cd harness/evals` and `fastbrowse-evals browser ...`.
- [ ] Dispatch the campaign: replace `PARALLAX_SHA: ${{ inputs.parallax_sha }}` with `HARNESS_SHA: ${{ needs.resolve.outputs.harness_sha }}`
  and `-f parallax_sha="$PARALLAX_SHA"` with `-f harness_sha="$HARNESS_SHA"`. Passing the SHA keeps the
  official rows and the campaign on one harness commit even if the dispatched ref moves.

## `.github/workflows/release.yml`

- [ ] Delete the `PARALLAX_SHA` env, the `test -n "$PARALLAX_SHA"` check and `-f parallax_sha="$PARALLAX_SHA"`.
  The release dispatches on its tag, so the official run's own commit is the harness.
- [ ] Delete the comment sentence naming `PARALLAX_SHA`, and the repository variable afterwards.

## Docs

- [ ] `AGENTS.md` lines 112 and 118: "Parallax moved its tasks into `dev`" becomes "its tasks moved into `dev`",
  and "the candidate schema that Parallax and the site share" becomes "that `evals/` and the site share".
- [ ] `docs/evals.md`:
  - line 3: "benchmark tasks run through the `evals/` project, a Parallax plugin";
  - lines 45 to 47: `evals/` owns prompts, truth functions and graders and checks the export;
  - lines 50 to 51: `public-tasks` was not ported (no workflow runs it). Either port it from the old
    `parallax/browser_use/public_tasks.py` or say the file is maintained by hand;
  - line 53: "the `evals/` harness commit";
  - line 98: point at `evals/` and `docs/agents/evals.md`.
- [ ] `docs/agents/evals.md`:
  - lines 13 to 16: the suite, adapters and official runner live in `evals/` (`fastbrowse_evals`). Run
    `uv run fastbrowse-evals browser internal --help`, `official --help`, `track-ledger --help` and
    `track-official --help` from `evals/`. `data` and `track` are gone;
  - line 69: the harness lock pins this checkout's fastbrowse, so workflows sync with
    `--no-install-package fastbrowse` and install the requested build over it;
  - line 82: "`fastbrowse-evals browser catalog` exports the internal catalog".
- [ ] `docs/agents/publish-evals.md`:
  - lines 3, 8, 22, 39, 60 and 129: "Parallax" becomes "the harness" or `fastbrowse-evals`;
  - lines 22, 39: `fastbrowse-evals browser publish-candidate`;
  - line 131: `fastbrowse_ref` and `harness_sha`;
  - line 135: `fastbrowse-evals browser track-ledger`.

## Publication

- [ ] The committed catalog's `task_sources["more_tasks.py"]` moved because the file imports from
  `fastbrowse_evals` now. A catalog that moves only its source hashes is a protocol change to the publication
  gate (`benchmarks._retirement_only`), so the first candidate after the move needs the owner's
  `protocol_change` approval. Task versions and fingerprints are unchanged.
- [ ] Every run checks that the measured build's packaged catalog matches the one the harness renders
  (`fastbrowse_evals.versions.provenance`). 0.10.0 ships the old hash, so the first build this harness can
  measure is the first one that contains the regenerated catalog.
- [ ] The candidate's `runner_sha` is now a fastbrowse commit. The publication check that it is in the
  checkout's history (`runner_pinned`) runs inside this repository, so it holds by construction.
