# Ten-repeat Ultrafast comparison

Run `575175e09100`, clean Fastbrowse commit `e7d61d216860254c70be45cdf19e5ae1fead75a8`.
Browser Use Ultrafast is pinned to 1231850a0bf1a0c0341fe408ef1668dbbfdfac46.
Six navigation tasks, ten repeats per task and arm, concurrency four, rotating arm order.
Both arms use OpenRouter. Native helper-model configurations differ; this is a product-configuration comparison.
Validation after fixes, separate from the immutable published 0.5.13 results.

| Arm | Passed | Median time | Median cost | Selected scored cost |
| --- | --- | --- | --- | --- |
| fastbrowse | 54/60 | 7.2s | $0.0026 | $0.2682 |
| jev-ultrafast | 39/60 | 11.6s | $0.0013 | $0.4602 |

## Per task

| Task | Fastbrowse | Ultrafast |
| --- | --- | --- |
| arxiv-open | 10/10 | 0/10 |
| flights-search | 10/10 | 0/10 |
| github-open | 10/10 | 10/10 |
| hn-comments | 4/10 | 10/10 |
| pypi-open | 10/10 | 10/10 |
| wiki-open | 10/10 | 9/10 |

## Retry accounting

127 physical attempts produced 120 selected rows. 0 task-repeat pairs excluded for exhausted outages.
Agent failures and slow completed calls remain scored. Earlier outage attempts remain in the ledger.
Attempt seconds sum work across overlapping sessions; they are not the duration of the whole invocation.
Scheduled retry waits are recorded separately. Unknown cost is never treated as zero.

- fastbrowse: 54/66 physical attempts passed; $0.3779 total reported cost; 870.8s summed attempt time; 660s scheduled backoff; $0.0070 per scored completion including outage spend.
- jev-ultrafast: 39/61 physical attempts passed; $0.4616 total reported cost; 1621.5s summed attempt time; 60s scheduled backoff; $0.0118 per scored completion including outage spend.

## Interpretation

Ten repeats measure run variability on these six tasks. They do not provide sixty independent tasks or establish broad workflow superiority.
No agent change was made in response to this comparison or the final heldout results.
The existing protocol accepts an agent-reported unavailable status as an outage. Fastbrowse reports HTTP 419 sessions this way; the Ultrafast adapter does not expose every equivalent browser HTTP status. All-attempt counts and spending are shown because outage-conditioned scores do not remove that observability limitation.
Two earlier partial batches were interrupted for harness classification fixes and are retained separately. They are not pooled into this frozen comparison; cancellation can leave incomplete cost records.

The [120 selected rows](2026-09-29-ultrafast.jsonl) and [127 physical attempts](2026-09-29-ultrafast-attempts.jsonl) retain grades, reported cost, timing and build provenance. Full traces remain in the run artifacts.

## Failures

Fastbrowse's six scored failures ended on the Hacker News homepage instead of a comments page.
Ultrafast exhausted its 50-step budget on all ten arXiv tasks and all ten flight tasks; the flight results
lacked the requested nonstop filter. Its remaining failure was on Wikipedia.

The browser change passed 39/39 dev attempts. Heldout was 26/27 before and 27/27 after, without tuning to
heldout failures. The browser fix is a generic duplicate-label regression, not proof that it caused every
change in this live-web result.
