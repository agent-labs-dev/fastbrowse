# Audited navigation comparison

Six live navigation tasks, ten repeats per arm. This core suite was used to diagnose failures and is not held out. The completion-fix validation passed 39/39 dev and 27/27 heldout; no heldout failures were used for tuning.

Run `986ae7a36a5b` used Fastbrowse 0.5.14 at clean `9600e5843405d12f4bb68d78f7b8879346b9050e` and [pinned Browser Use Ultrafast](https://github.com/browser-use/jev-ultrafast/tree/1231850a0bf1a0c0341fe408ef1668dbbfdfac46), rechecked as upstream HEAD on 2026-09-29. The two-arm PyPI preflight passed separately and is not pooled into these scores.

Both used Browser Use Cloud with resizing enabled, a 1120 by 780 CSS-pixel viewport at device scale 1, the same task prompts and concurrency 4. Every retained final-page measurement was checked. Ultrafast evidence was read on its own tab before its driver disconnected. Resizing can reduce cloud browser stealth; it was enabled for both arms.

Arm launch order rotated between repeats. Fastbrowse used Gemini 3.5 Flash Lite and Gemini 3.8 Flash, with URL shortcuts and its configured Jev backup. Ultrafast retained its pinned policy and the upstream example text-helper configuration, inception/mercury-2.5 with reasoning disabled. This compares product configurations, not matched helper models. The limits were 50 Fastbrowse steps and 50 Ultrafast executed actions, with 100 Ultrafast decisions; stale choices did not spend its action allowance.

## Results

149 physical attempts produced 120 selected results. 2 unavailable task/repeat pairs were excluded from both arms. Agent failures remained scored. Medians below include passing and failing scored runs; they exclude earlier outage attempts and retry waits.

| Arm | Passed/scored | Median scored-run time | Median scored-run cost | All physical attempts | All reported attempt spending | Scheduled retry waits |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Fastbrowse | 58/58 | 8.3s | $0.00256 | 77 | $0.34591 | 3600s |
| Browser Use Ultrafast | 38/58 | 12.9s | $0.00136 | 72 | $0.76610 | 1440s |

All-attempt spending sums reported model, browser and proxy charges, including discarded outage attempts. It is not a reconciled invoice total. Two Ultrafast provider-error requests returned no charge information before the adapter raised; their rows retain earlier reported charges, but the failed-request charges are unknown. The raw adapter flags do not capture this gap; the row-by-row audit annotates it without rewriting the raw evidence. These sums can therefore understate actual spending. Neither provider-error attempt enters the scored-run cost median. Scheduled retry waits are reported separately from attempt timing; these medians are not end-to-end time including queueing and retries.

| Task | Fastbrowse passed/scored | Ultrafast passed/scored |
| --- | ---: | ---: |
| `arxiv-open` | 10/10 | 0/10 |
| `flights-search` | 10/10 | 0/10 |
| `github-open` | 10/10 | 10/10 |
| `hn-comments` | 8/8 | 8/8 |
| `pypi-open` | 10/10 | 10/10 |
| `wiki-open` | 10/10 | 10/10 |

HN produced 19 unavailable physical attempts for Fastbrowse and 10 for Ultrafast. Both arms clicked comments links and encountered both HTTP 200 content and HTTP 419 documents at the same story URLs. The error documents contained only "Sorry". The cause was not isolated; excluding these responses does not establish that they were independent of the agents. The ledger retains them, and the paired exclusions above must be read alongside the scored results. The HN grader accepts the comments page of any of the leading five stories because the front page can reorder during a run.

| Arm | Selected unavailable results |
| --- | ---: |
| Fastbrowse | 2 |
| Browser Use Ultrafast | 0 |

## Before retries

These are the first physical attempts, before retry recovery or paired exclusions. Unavailable is reported separately from agent failure; no outage is relabeled as an agent failure.

| Arm | Passed immediately | Scored failures | Unavailable |
| --- | ---: | ---: | ---: |
| Fastbrowse | 54 | 0 | 6 |
| Browser Use Ultrafast | 34 | 18 | 8 |

## Failure audit

Every failed physical attempt, including retries, has a diagnosis in the [row-by-row audit](2026-09-29-final-failure-audit.json). The [full attempt ledger](2026-09-29-final.attempts.jsonl) preserves status, final document evidence, decisions, stale choices, timing, spending, retries and provenance. No failed attempt was silently replaced by a pass.

| Diagnosis | Physical attempts |
| --- | ---: |
| Stops dialog omits transparent native radio controls | 10 |
| Observed HTTP 419 document; origin unknown, bounded site-outage retry | 10 |
| Covered Search control chosen repeatedly; native executor rejects it | 10 |
| Fastbrowse code recorded HTTP 419 at HN comments and ended unavailable; observed status retained, origin unknown | 19 |
| Provider returned retryable 504 error inside HTTP 200; adapter rejected that choice before execution and retained the attempt | 2 |

Ultrafast's arXiv and Flights failures were checked against the offered controls and retained histories, with independent browser probes and recorded diagnostics in the [fairness audit](2026-09-29-fairness.md). Its policy and snapshot code were not patched to change its measured behavior. HTTP 419 is observed evidence; its site, proxy or browser-service origin remains unknown. Browser IPC timeouts do not establish an agent failure and have one bounded retry. Exhausted unavailable pairs are excluded symmetrically.

## Limits and publication

Ten repeats measure variability on these six tasks. They do not establish general workflow superiority, a universal success rate, or a universal cost advantage. Navigation is narrower than Fastbrowse's cited-answer, structured-output, credential, authorization and spending-control APIs. Those features are described from code and tests rather than inferred from this score.

Earlier bug-finding and interrupted candidates remain [diagnostic evidence](2026-09-29-fairness.md); none is pooled into this run. Interrupted candidates can have unreported in-flight spending. The public README, eval tables and website figures are generated only from the approved final rows.
