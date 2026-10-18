# Publish benchmark results

Full browser campaigns run through Parallax. Its exporter validates clean builds, scheduled slots, source
identity, physical ledgers and private Langfuse read-back before producing `docs/results/benchmark-candidate.json`.
The file contains sanitized per-run grades, completion, time and cost. Official rubric scores retain
their source meaning. Internal hosted and Ultrafast comparisons use separate matched task groups.

Schema 1 requires all four original groups. Schema 2 accepts nonempty subsets of those four groups.
Schema 3 also supports `internal-fastbrowse`: every supported internal task in the current catalog (53 since
`stretch-wizard-correction` was retired; figures approved earlier ran 54), three repeats each, with
fastbrowse alone. The fastbrowse-only group does not require comparator reruns. Official
fastbrowse suites do not require new comparator runs. Previously published comparator figures can be linked
as references with their task and build differences stated. Every group inside one candidate still comes
from the same measured agent and runner build. Adding a suite must retain the published groups and their
regression checks; results from different builds cannot be combined under one build receipt.
Schemas 2 and 3 record each group's agent and runner commits and require them to match the candidate's build.

CI validates the candidate's exact coverage and compares matched scores, time and cost with the previous
approved candidate. A group whose catalog differs from the approved one only by tasks listed under the catalog's
`retired` (a site that is gone) is compared on the tasks both ran; the earlier catalog is found in Git history by
its digest. Any other catalog, task or limit difference is a protocol change, published only as described
under [Changing the protocol](#changing-the-protocol). Unresolved costs, missing grades, incomplete coverage and
a failed fixture workflow block publication. The fixture receipt is a successful maintainer dispatch of
`evals.yml` whose head commit is the measured build. When `main` has moved past that build, push a branch at
it and dispatch the workflow there; a branch run counts only when its commit is in `main`'s history, so the
workflow and runner it used are ones `main` held. These checks detect recorded regressions; they do not prove that
every website or grader is free of bugs. Review unexpected outcomes before approving.

To approve reviewed results, dispatch `publish-evals.yml` on `main` with the SHA256 of the exact candidate
bytes. For the first publication, also approve establishing the initial baseline. Only the maintainer can issue
this approval. The workflow revalidates coverage and the fixture receipt and returns a `benchmark-approval`
artifact. Download `benchmark-approval.json` and install it with

```sh
uv run python -m fastbrowse.evals.benchmarks --candidate docs/results/benchmark-candidate.json --install benchmark-approval.json
```

which copies the reviewed candidate to `docs/results/benchmarks.json` and the receipt to
`docs/results/benchmark-approval.json`. Include both in the reviewed data PR and wait for green CI before
merging. An upload to Langfuse does not approve publication.

## Retiring a task or changing the protocol

Neither path exists to improve a score. A task is retired only for an external reason that stops it measuring
any agent, such as a site that no longer exists, and its catalog entry records that reason, with evidence
(for example, the DNS answers showing the name is gone) and a date. A task is never retired, swapped or
regraded because of how an agent scored on it, and a protocol change never drops or replaces tasks for that
reason. A protocol change states its cause in its note, such as #356's uncapped runs.

## Changing the protocol

A published group is compared only with figures measured under the same protocol: the same catalog digest,
tasks and limits. Changing a limit note cannot skip regression checks. When the protocol changes on
purpose (a task or grader version, an arm, a limit, such as the uncapped protocol of #356), the changed groups
cannot be compared with their published figures and are published as a fresh baseline instead:

1. Stage the candidate as usual. CI reports each staged group whose protocol changed and still compares the
   others, including a group that differs only by listed retirements. A group both retired from and measured
   under a new protocol, such as the first uncapped internal-fastbrowse figures, is a protocol change.
2. Dispatch `publish-evals.yml` with `protocol_change` set and a `protocol_note` saying what changed and why.
   The workflow refuses the dispatch when no published group changed protocol. Without the flag, a changed group
   is refused as before. Every published group must still be present, cover the current catalog exactly
   and pass the fixture receipt; groups whose protocol did not change are still checked for regressions, and a
   group that differs only by listed retirements is still compared on the tasks it shares and left out of the
   record.
3. The approval carries a `protocol_change` record: the note, the digest of the figures it supersedes, and
   for each changed group its previous and new catalog digest, tasks and limits.
4. `--install` archives the superseded figures as `docs/results/benchmarks.<campaign>.json` and their
   approval as `docs/results/benchmark-approval.<campaign>.json` before putting the new figures in place.
   `<campaign>` is the measuring campaign's id, kept to 64 filename-safe characters, then the first 16 hex
   characters of the superseded figures' own SHA256, so a reused campaign id with other figures gets its own
   name and CI can check each archived file against its name. Follow the paths the record stores rather than
   rebuilding them. The record links both, so each approval leads to the figures
   it replaced.

CI accepts changed published figures only when their approval's record names exactly the groups whose protocol
changed, from exactly the figures at the base, and the archive holds those figures and their approval byte for
byte. A new approval receipt must also be the exact bytes of the `benchmark-approval` artifact its owner run
produced, so its note and record cannot be edited after download; `--install` copies it unchanged. Archived
figures are published results: CI refuses any change to them or their removal, and any new archive file other
than the pair a validated protocol change names.

The website reads the candidate and approval from one resolved fastbrowse commit. It checks the exact-byte
digest and the completed owner-dispatched workflow receipt before deriving any figure. A changed
candidate needs a new approval. Raw answers, page content, recordings and private trace links stay in
Langfuse and ignored local artifacts.
