# Native corpus diagnosis

The retained WindTunnel draw scored Fastbrowse 23/36 native passes and Browser Use 33/35. Fastbrowse completed
21 attempts and passed both native correctness and completion in 19. The arms used their default models, not
the same model. One missing Browser Use grade prevents a complete paired comparison. The original grades and
all physical attempts remain unchanged in the [comparison report](2026-10-05-fresh-comparisons.md).

## Causes and general fixes

| Observed failure | General fix | Boundary retained |
| --- | --- | --- |
| Three access-boundary inspections stopped at sign-in | A task-only plan flag permits reading an observed access wall for an information task | Protected retrieval still requires login; bot checks remain blocked; answers need quotes |
| Checkout validation changed field labels; email and phone were confused | Index input name and autocomplete, and bind a supplied recovery value to the observed field and document | Secrets use the existing origin-scoped path; a redraw invalidates the binding |
| Cart and menu overlays covered navigation controls | Report visible obstruction controls to recovery | Clicks still cannot pass through a cover; a description race keeps the covered result |
| Group headings were counted as activities | Route record counts through quoted record extraction rather than scalar choice | A count still needs its basis facts |
| Ticket checkout lost cookies at the public endpoint | Rewrite cookies naming the configured upstream host to the configured public host | Security attributes stay intact; unrelated domains stay untouched |
| Booking and cancellation could duplicate a transaction before reading its outcome | Read a possible state-changing submission's outcome before another interaction | Native dialogs are handled before capture; completion still needs verification |
| The upstream cancellation grade checked customer count, not the requested sequence | Add separately labelled, pinned before/during/after native witnesses | Missing witnesses are ungraded; the unrecorded cancellation reason cannot pass |

These changes add no task, site or expected-answer detection to the agent. Corpus-specific expectations are
data in the independent [state supplement](2026-10-06-native-state-supplements.json). The inspected corpus is
now diagnostic material, not a fresh held-out draw.

The endpoint and grader review also fixed a request-target authority escape, credential forwarding through
HTTP redirects, origin-prefix rewrites, header encoding and boolean/numeric equality. Watcher samples stay
outside agent output. Reset, sampling and grading overhead stay outside the measured arm latency. The upstream
grader and observer require reviewed code digests; changed supplement contents change the grading identity.

## Diagnostic evidence

All live runs below used three repeats and a $0.04 per-attempt cap. They are diagnostics, not publication
evidence. The provisional branch runs used a dirty working tree and preceded the final review fixes.

| Run | Passed | Recorded dollars | Runtime |
| --- | --- | --- | --- |
| Clean main held-out reference | 21/27 | 0.39176 | Python 3.14 |
| Clean main held-out reference | 23/27 | 0.38907 | Python 3.13 |
| Provisional changes, dev | 39/39 | 0.37667 | Python 3.13 |
| Provisional changes, held-out | 22/27 | 0.36051 | Python 3.13 |

The aligned Python 3.13 comparison is 23/27 versus 22/27. These runs do not establish an improvement and do
not replace any published figure. Held-out failures were not used to design or tune the fixes.

A local fixture run was interrupted after five passing attempts when the new transaction-read guard tried to
capture while a native confirmation dialog had JavaScript paused. Its finished rows retain $0.00741; the
in-flight cost is unknown. A regression reproduces that ordering failure. After exempting an open native dialog
from pre-interaction capture, the real confirmation-dialog task passed three times, costing $0.00249.

Focused verification covers access walls, field correction, record totals, covered targets, observer isolation,
state sequence grading, credential redirects and endpoint forwarding. CI runs the complete test gate. Every
release still requires green three-repeat fixture Evals on its exact commit; a new comparison also needs a
complete paired draw and the publication gate. No native improvement figure is published from this diagnosis.

The broad capped local fixture run finished 95/96, costing $0.92037. One infinite-scroll attempt retained 40 of
60 records and stopped stuck. This is a failure, not a passing partial answer. Subsequent inspection found that
reusable tally extraction accepted record blocks but excluded list items that ordinary tally extraction accepts.
The list-item regression failed before aligning those paths; all 229 retrieval tests passed afterward. The
original failed attempt remains in the diagnostic receipt. These local results cannot authorize publication.

The first three-run recovery check passed 1/3, costing $0.08652. The list-item fix alone did not resolve the
failure. A directly quoted whole-list total was also suppressed whenever earlier partial tallies existed.
Direct page statements now retain their quotes for the normal claim verification; derived totals still need
complete tally evidence. The direct and derived cases have separate regression coverage. After this fix the
task passed 3/3, costing $0.07385. All 232 retrieval tests pass, including a quoted-count check that rejects an
unsupported number. Both recovery checks remain recorded.

Post-push review fixed lowercase origin headers that bypassed endpoint rewriting and repeated forced reads of
blank transaction outcomes. A blank outcome proves nothing but permits recovery to navigate. CI's secret scan
now checks the build's complete ancestry: a fresh clone reproduced its false positive on an artificial token in
an unrelated contributor branch, while the PR branch and its merge commit had no findings.
