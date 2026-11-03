# eval-tuning

## Defect

Product behaviour shaped to a benchmark rather than to the task: a special case for a benchmark task, a
named site or domain, a page's exact wording, a selector or element id, or a known answer, anywhere in
product code, prompts, Jev questions, defaults, retry or recovery logic. Also a default, threshold or
step budget chosen because it scored on one benchmark, with nothing but that benchmark behind it. The
agent passes the eval and fails the same task on a site the eval does not name.

## Do not flag

- Tests, fixtures, cassettes, mock sites and eval harness code (`src/fastbrowse/evals`, `tests`,
  `benchmarks`, `bench`): they are meant to name tasks, sites and answers.
- Site knowledge that is a documented, general web contract: a consent-banner library's selectors, a
  well-known login form field name standard, an RFC-defined header.
- Examples in user-facing documentation that name a public site to illustrate usage.

## Judgment

Would this branch, string or value exist if the benchmark did not? Does it help the same task on an
unnamed site, or only the one the eval runs?

## Output

On the special case. `what` names the benchmark, site, wording or answer it encodes; `why` names
which eval it matches and what the agent does on a site the eval does not name.
