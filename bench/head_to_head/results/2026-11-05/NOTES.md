# Notes on campaign 2026-11-05

What `runs.jsonl` cannot show on its own.

- **Interruption.** The job driving the campaign hit a 1920 s limit during the last sensitivity run,
  `saucedemo-checkout.browser-use-default-effort.3`. That run left no result, and it was run again at 19:12 UTC by
  resuming the campaign. All 36 head-to-head runs had finished before this and were not rerun.
- **No retries, no replacements.** Every head-to-head run is its first and only attempt.
- **Cost check.** fastbrowse's reported total of $1.2450 was reconciled against the `usage.cost` OpenRouter returned on
  each response in its traces: $1.2440. The gap is one Jev call per checkout run, with no response in the trace, which
  fastbrowse priced as `estimated` ($0.00033 each). Its text-model lines match the rate OpenRouter reported to Browser Use's
  meter, $0.75 and $3.75 per million input and output tokens.
- **Answers.** Every answer was also read by hand against the grader's verdict, and they agree.
- **Why Browser Use took 3 steps on the aggregation tasks.** It wrote one `evaluate` call that fetched and tallied every page
  in the browser, so the work is real, in one action. See `raw/stretch-quotes-top-authors.browser-use.1/stderr.txt.gz`.
- **fastbrowse's two failures** are `stretch-quotes-top-authors` runs 1 and 3, both ending `unverified` ("A composed claim
  is unsupported by its cited quotes") after 110 and 108 steps. Run 1 returned no answer. Run 3 returned Einstein 10 and Rowling
  9, omitted Monroe, and said its evidence was partial. Run 2 passed in 23 steps.
- **Machine load.** The 1-minute load on this 4-vCPU machine stayed between 0.25 and 1.78 across the head-to-head.
- **Traces.** fastbrowse's traces are 57 MB unpacked and 8.2 MB packed, so they are not in the repo. They were delivered with
  the results as `fastbrowse-head-to-head-2026-11-05-traces.tar.gz`.
