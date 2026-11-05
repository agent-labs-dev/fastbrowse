# arbitrary-caps

## Defect

A limit on what a model, a grader or a trace sees, with no documented provider limit behind it: page text
sliced to N characters, `text::prefix(.., N)` on content a model reads, a top-N or `take(N)` over controls,
claims, quotes or history, an onscreen or offscreen control quota, an output token ceiling, a cap on
questions, options or blocks below what the provider documents, evidence or trace fields cut short. The
model or grader works from less than the page holds and fails in ways that look like model error.

## Do not flag

- A limit the provider documents, named as that provider's limit and read from its profile (Jev's token,
  option and question limits in `DecisionProfile`, a host's request size).
- Error messages and log lines shown to people only, where the full value is kept elsewhere in the trace.
- Bounds on untrusted input that protect the process and never reach a model (CDP frame size, file
  download size), when the bound is far above any real page.
- Timeouts, retries and step budgets the user sets through `Limits`: those are run limits, not data caps.

## Judgment

If the page, document or reply were larger than this number, would a model or grader see less than it
needs? Is the number a provider's documented limit, or a value someone picked?

## Output

On the line that applies the cap. `what` names the data cut and the number; `why` names who reads the cut
value (which LLM purpose, Jev question, grader or trace) and whether any provider documents the limit.
