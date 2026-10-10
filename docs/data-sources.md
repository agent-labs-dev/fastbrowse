# Benchmark sources

fastbrowse retains source identities in `fastbrowse.evals.sources`. Parallax owns dataset loading, official
grader adapters and benchmark execution under its `browser-use` family.

BU Bench uses the pinned Browser Use benchmark dataset and its weighted findings grader. The encrypted
tasks stay private. The current full task set and a historical subset are separate measurements.

Online-Mind2Web uses the authors' dataset and WebJudge protocol, including browser screenshots and action
evidence. Agent final answers are not passed to WebJudge. Source revision, dataset digest, judge model and
protocol are recorded with each run.

WindTunnel remains a diagnostic source. Its native state predicates can cover only part of a task;
source-predicate success and verified task completion are reported separately. A stronger independent
state check is identified as an additional check, not as the upstream grade.

Published reference figures retain their source and configuration limits. They are not silently treated as
matched reruns. See [measurement methods](evals.md) and [the eval workflow](agents/evals.md).
