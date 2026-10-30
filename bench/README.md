# Benchmark

This directory holds the cassettes and results for `src/fastbrowse/evals/bench.py`. The harness runs
every fixture task (the `local` and `mock` suites) against any program that speaks `fastbrowse serve --stdio`, with
the model traffic played back from a cassette. It checks two things: given the same model responses, the agent
reaches the same results, and how much time, CPU and memory it takes to get there.

## How a task runs

For each task the harness:

1. serves the fixture site on a fixed port (47300 plus the task's position in the suite), because the address
   reaches the prompts and the model writes it back;
2. starts one headless Chrome and hands it to the agent as `cdp_url`, so Chrome's CPU and memory are not counted
   against the agent;
3. starts a proxy on a free port and points the agent at it with `FASTBROWSE_DECISION_BASE_URL` and
   `FASTBROWSE_LLM_BASE_URL`;
4. starts one agent process, sends `initialize` and `run`, answers `secrets/resolve`, and grades the result with
   the suite's own checks.

The proxy files each response under the sha256 of the request path and the request body in canonical form. Keys
are sorted and JSON held in a string is parsed. Fixture ports, capture dates, timestamps and screenshots become
placeholders. Chrome target ids are numbered in the order the request names them, and the response gets the new
ids back. List order counts, so an implementation that writes a set in another order misses. Headers are left
out, and the bearer token with them. A request the proxy has no response for gets HTTP 404 and is logged as a
miss, with a diff against the nearest recorded body. A miss means the agent asked something the recording never
asked.

## Commands

Every command takes `--agent "<command>"`. The default is this checkout's `.venv/bin/fastbrowse serve --stdio`;
a Rust build is `--agent "$PWD/target/release/fastbrowse serve --stdio"`. `--agent in-process` calls `fastbrowse.run_task`
in the harness's own process instead, on the Rust core through
`fastbrowse.native`. It measures no startup, and its CPU and peak RSS are the harness's.

```sh
# Record every task against OpenRouter. Only the proxy gets the key; the agent gets a placeholder.
OPENROUTER_API_KEY="$(security find-generic-password -s openrouter-api-key -w)" just bench record

# Replay what is recorded and send only the misses upstream, until a run at zero latency asks nothing new.
OPENROUTER_API_KEY=... just bench fill

# Replay. --latency recorded holds each response as long as the provider took.
just bench replay --out artifacts/bench/a.json
just bench replay --latency recorded --out artifacts/bench/b.json

# Diff the normalized results of two runs. Exits 1 when any task differs.
just bench compare artifacts/bench/a.json artifacts/bench/b.json

# Three replays at zero latency, one at recorded latency, and the report.
just bench-baseline artifacts/bench/baseline
```

`fill` exists because the agent asks for its plan while Chrome starts and takes its first step without it if the
plan has not come back. Recorded live, the plan took about three seconds and the first Jev call carried no
requirements. Replayed at zero latency, the plan is back first and the same call carries them. `fill` records that
second path too. Because of the same race, a zero-latency run and a recorded-latency run take different paths on a
few tasks; compare runs made at the same latency.

## What is compared

`compare` diffs, per task: status, answer, structured data, error, final URL, citations (URL, quote, text), each
step's operation, target, outcome and note, the grade, and the replay misses. It ignores timings and costs, and
prints cost changes on their own lines. It refuses a run file that has no `finished` time, since the file is
rewritten after every task.

`mock-support-portal` depends on wall time. Its page shows a modal 1.2 seconds after load, and the step the
modal lands on depends on how fast the agent is, so its result changes between runs and implementations. Its
diffs are labelled timing-sensitive. Every other task gave the same result in three zero-latency replays.

## What is measured

| metric | meaning |
|---|---|
| startup | spawn to the `initialize` reply |
| wall | spawn to the `run` reply, Chrome start not included |
| CPU | user plus system time of the agent process and anything it waited for, from `wait4` |
| peak RSS | `ru_maxrss` of the agent process, from `wait4` |
| steps | steps in the result |
| installed size | fastbrowse's source and its runtime dependencies in the venv, without extras or bytecode |

Timings come from a shared laptop and vary run to run. Compare medians from runs on the same machine.

## Files

- `cassettes/<task>.jsonl.gz`: one gzip JSONL file per task. The first line holds the recording date, version and
  models; every other line is one request and its response, in the order they arrived.
