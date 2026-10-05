# Eval data sources

External tasks are fetched into the user cache, never committed here. Tests use synthetic tasks authored for
fastbrowse. A digest mismatch stops loading, including when the bad bytes came from the cache.

## online-mind2web

- Name: Online-Mind2Web, OSU NLP Group.
- Upstream: <https://huggingface.co/datasets/osunlp/Online-Mind2Web>.
- Pinned revision: `eacad896a84dc5b65e29b0b06e4699ab0544d701`.
- File: `Online_Mind2Web.json`.
- SHA-256: pending gated access. The loader refuses to download without a verified digest supplied through
  `--sha256`. No digest has been invented or learned from an unverified first download.
- The public upstream tree pins this file's Git blob to `e8a5e5a99f2be9eae14f4e4259bd5af562f80da9`;
  the loader checks that identity as well as the supplied SHA-256.
- Licence: [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), declared in the
  [dataset card](https://huggingface.co/datasets/osunlp/Online-Mind2Web/blob/eacad896a84dc5b65e29b0b06e4699ab0544d701/README.md).
- Attribution: Xue et al., *An Illusion of Progress? Assessing the Current State of Web Agents* (2025),
  and Deng et al., *Mind2Web: Towards a Generalist Agent for the Web* (2023). The card requests both citations.
- Fetched, not vendored. Hugging Face requires acceptance of the dataset's access conditions and an authorized
  `HF_TOKEN`. The card describes release for research purposes and opposes harmful uses. CC BY 4.0 itself does
  not impose a non-commercial restriction; retain attribution and check the access terms before downloading.

## windtunnel

- Name: WindTunnel, nekuda.
- Upstream: <https://github.com/nekuda-ai/WindTunnel>.
- Pinned revision: `5ca8644e23826ebb30108e7bad240b61043bfe67`.
- Artifact: [repository archive](https://codeload.github.com/nekuda-ai/WindTunnel/tar.gz/5ca8644e23826ebb30108e7bad240b61043bfe67).
- SHA-256: `9254737b8062a4a7140ec2a91f3b111abe648bd6d649b2775d2e68ad1f054d82`. GitHub does not promise
  generated archives stay byte-identical; if the digest stops matching, verify the commit's tree and re-pin
  rather than trusting the new bytes.
- Licence: [Apache-2.0](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/LICENSE)
  for WindTunnel's own task definitions and harness code.
- Attribution: WindTunnel by nekuda; site selection and reference tools by Ilay (nekuda). See the pinned
  [ATTRIBUTION.md](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/ATTRIBUTION.md).
- Fetched, not vendored. The loader reads task YAML from the verified archive without extracting or executing
  upstream code. Calibration tasks are excluded. Running a site or its grader is a separate setup step.
- The sites retain their own MIT, AGPL-3.0 or GPL-3.0 licences. Hi.Events also requires its attribution footer.
  Upstream lines in patches retain their original licences. Fetching the archive does not relicense them;
  preserve the notices and consult upstream attribution before running or redistributing a site.

## Running a corpus

`fastbrowse.evals.corpus` executes a pinned corpus. It fetches through the loaders above, so the bytes are checked
the same way there, and it never runs upstream code. Selection uses a seed and a per-stratum count, and the drawn
tasks are hashed with their source pin into one corpus digest, so a run names exactly what it drew.

```sh
uv run --extra eval-data python -m fastbrowse.evals.corpus windtunnel \
  --site-urls sites.json --per-stratum 1 --out artifacts/evals/windtunnel
```

The default writes `corpus.json` and `preflight.jsonl` and calls no agent. Preflight is an HTTP load check of each
start address. It says whether a page answers, and nothing about whether a task is feasible, needs a login or meets
a bot check; those still need a person or a browser. Preflight exits nonzero when any start address is unreachable.

Prerequisites: the gated Online-Mind2Web file needs `HF_TOKEN` and the verified `--sha256` the loader insists on,
and WindTunnel needs `--site-urls` mapping each site id to a running local address.

`--execute` adds `attempts.jsonl` and `summary.json`, runs the agent, and grades each attempt. Each attempt keeps
two things apart:

- `completion` is what the agent reported about its own run, normalised from its status.
- `graded` and `grade` record an independent judgement, named by a grader and its version.

An attempt with no grader is written ungraded, with `passed: null`. That means the pass state is unknown, not
false, and it is not a benchmark score. A pass needs the grader's pass and the agent's own completion. A grader
that raises leaves its attempt ungraded and records why. The command exits 0 only when every expected attempt ran
and passed; an ungraded, failed or missing attempt is nonzero.

WindTunnel answer predicates are scored natively from the pinned predicate, and an ungraded attempt names the
predicate when it cannot be scored. An action predicate needs a live state probe, so without a trusted evaluator it
stays ungraded rather than counted as a failure. Online-Mind2Web rows carry no predicate, so they are graded only
by a trusted evaluator.

A trusted evaluator is a command the caller deliberately installed and selected:

```sh
uv run --extra eval-data python -m fastbrowse.evals.corpus online-mind2web \
  --sha256 <digest> --out artifacts/evals/om2w --execute \
  --grader-code my_grader.py --grader-sha256 <sha256 of my_grader.py> \
  --grader-command uv run python my_grader.py
```

`--grader-command` takes the rest of the command line as one argument vector, so give it last. Its stdin receives
one JSON request (the pinned task and the observed attempt) and its stdout must be one `Grade`. `--grader-code`
names the file whose sha256 pins the command; pass the evaluator file, not its launcher, and that digest is
re-checked on every call, so an edited evaluator is refused. `--grader-sha256` requires the pinned digest up front.
`--grader-timeout` bounds one call. The command inherits an allowlisted environment, so provider keys and remote
credentials never reach it.

The digest pins the evaluator file. Its interpreter, imported modules and data files must be pinned by
the evaluator's own environment.

Each paid attempt keeps its trace under `downloads/<task digest prefix>/repeat-<n>/`, including the full
`run-result.json` from `run_task`; the attempt row records that path as `run_artifact`. An `attempt.json` marker
identifies a run interrupted before it returned a result. The command requires a fresh output directory. A run interrupted
partway still writes `summary.json` for the attempts that finished, and exits nonzero.

## Authored fixtures

The static HTML in `src/fastbrowse/evals/fixtures/` and the synthetic test data in `tests/evals/` are authored
for fastbrowse and remain in this repository under its [MIT licence](../LICENSE). They contain no copied
Online-Mind2Web or WindTunnel task rows.
