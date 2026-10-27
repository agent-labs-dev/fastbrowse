# Rust against Python on the fixture suites

The Rust `fastbrowse` binary and the Python package both ran `serve --stdio` under the benchmark harness
(`src/fastbrowse/evals/bench.py`, described in `bench/README.md`). Each replayed the same committed cassettes in
`bench/cassettes`, so both agents got the same model responses for the same requests.

This run is on fastbrowse 0.7.3. Everything the 0.7.1 run covered is still ported, and so is what main added
since: the dropped post-answer audit and Jev's later list pages (#445), the partial comparison settled by the
complete one beside it (#453), the click that downloads a file counted as a page change (#441), and `--fetch`
(#454), which no fixture task uses. #445 and #453 changed what both agents ask, so `mock-priciest-product` and
`mock-infinite-scroll` were filled again at both latencies, and `mock-report-total` and `mock-newsletter-gate` at
recorded latency, for $0.0007 in all.

## Result

- **Same outputs.** At zero latency every Rust run matches the Python run beside it on all 36 tasks (6 local, 30
  mock): status, answer, data, error, final URL, citations, grade and every step. At recorded latency they match
  on all 36 too.
- **No new requests.** Neither agent asked anything the recording never asked: 0 replay misses in all eight runs.
- **In process too.** `bench replay --agent in-process` at zero latency gives the same result on all 36 tasks for
  the Python core and for the Rust core through `fastbrowse_native`, and both match the `serve` runs.
- **Every run agrees with itself.** The three zero-latency runs of each agent are identical to each other.
  `mock-support-portal` ends stuck in every zero-latency run of both agents and completes at recorded latency.
- **#441 was a real divergence.** main's Rust binary, without the port of #441, ended `mock-invoice-export` in
  `error` after one request Python never sends. With the port it completes in the 13 steps Python takes.
- **Fewer calls.** #445 and #453 took a zero-latency replay from 635 model requests to 516 in both agents, and a
  recorded-latency one from 613 to 494.
- **CPU, startup and memory are well past 2x. Wall time is not.** At zero latency Rust uses 18x less agent CPU,
  starts 54x faster and peaks at 5.4x less memory. Wall time is 1.1x better in total, and 1.0x at recorded
  latency, where the model's own response time dominates.
- **Footprint.** The `release-small` binary is 4.1 MiB, against 13.0 MiB of Python packages (before the
  interpreter) and the 50.4 MiB npm package that carries the PyInstaller build.

## Zero latency, 3 runs each

Medians across the 3 runs per task, then the median and total across the 36 tasks. Rust is the `release` build;
each Rust run followed the Python run it is compared with.

| metric | Python | Rust | Python / Rust |
|---|---:|---:|---:|
| wall s, median of tasks | 1.83 | 1.54 | 1.2x |
| wall s, total | 107.06 | 96.18 | 1.1x |
| agent CPU s, median of tasks | 0.35 | 0.02 | 19.9x |
| agent CPU s, total | 13.25 | 0.73 | 18.2x |
| startup s, median of tasks | 0.199 | 0.004 | 54.5x |
| startup s, total | 7.184 | 0.132 | 54.3x |
| peak RSS MiB, median of tasks | 73.2 | 13.6 | 5.4x |
| peak RSS MiB, max | 74.8 | 16.2 | 4.6x |
| steps, total | 219 | 219 | - |
| model requests, total | 516 | 516 | - |

The laptop ran nothing else heavy during these runs. Each Python run and each Rust run started a fresh agent per
task against one headless Chrome the harness owned.

| task | pass Py | pass Rs | wall Py | wall Rs | CPU Py | CPU Rs | CPU | startup Py | startup Rs | RSS Py | RSS Rs |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 3/3 | 2.59 | 2.33 | 0.35 | 0.02 | 20.4x | 0.201 | 0.004 | 73.0 | 13.2 |
| contact-form | 3/3 | 3/3 | 2.30 | 1.98 | 0.34 | 0.02 | 19.7x | 0.199 | 0.003 | 72.9 | 12.6 |
| contact-needs-confirmation | 3/3 | 3/3 | 2.00 | 1.69 | 0.35 | 0.02 | 21.5x | 0.197 | 0.003 | 72.9 | 12.3 |
| table-extract | 3/3 | 3/3 | 0.59 | 0.33 | 0.30 | 0.01 | 37.4x | 0.196 | 0.004 | 73.0 | 12.9 |
| login-wall | 3/3 | 3/3 | 0.58 | 0.33 | 0.29 | 0.01 | 45.3x | 0.199 | 0.004 | 72.8 | 11.2 |
| confirm-dialog | 3/3 | 3/3 | 0.87 | 0.63 | 0.32 | 0.01 | 37.1x | 0.201 | 0.004 | 73.0 | 12.0 |
| mock-cold-sign-in | 3/3 | 3/3 | 14.16 | 13.89 | 0.35 | 0.02 | 20.2x | 0.196 | 0.003 | 73.0 | 13.6 |
| mock-supplied-sign-in-route | 3/3 | 3/3 | 14.43 | 14.12 | 0.38 | 0.02 | 18.5x | 0.205 | 0.004 | 73.2 | 13.6 |
| mock-sign-in | 3/3 | 3/3 | 1.99 | 1.68 | 0.37 | 0.02 | 18.3x | 0.202 | 0.004 | 73.2 | 13.6 |
| mock-sign-in-two-step | 3/3 | 3/3 | 3.03 | 2.73 | 0.42 | 0.03 | 13.7x | 0.200 | 0.004 | 73.5 | 13.8 |
| mock-sign-in-code-given | 3/3 | 3/3 | 2.72 | 2.39 | 0.39 | 0.02 | 16.3x | 0.203 | 0.004 | 73.5 | 13.7 |
| mock-sign-out | 3/3 | 3/3 | 2.00 | 1.70 | 0.37 | 0.02 | 17.5x | 0.205 | 0.004 | 73.1 | 13.7 |
| mock-basket | 3/3 | 3/3 | 3.47 | 3.13 | 0.44 | 0.03 | 13.2x | 0.199 | 0.004 | 73.5 | 14.0 |
| mock-order-validation | 3/3 | 3/3 | 4.82 | 4.37 | 0.50 | 0.04 | 11.3x | 0.205 | 0.004 | 74.0 | 14.1 |
| mock-order-pause | 3/3 | 3/3 | 3.70 | 3.32 | 0.43 | 0.03 | 14.6x | 0.200 | 0.004 | 73.4 | 12.6 |
| mock-password-change | 3/3 | 3/3 | 3.13 | 2.77 | 0.41 | 0.03 | 13.1x | 0.199 | 0.004 | 73.5 | 13.5 |
| mock-password-pause | 3/3 | 3/3 | 2.82 | 2.48 | 0.39 | 0.02 | 16.5x | 0.198 | 0.004 | 73.3 | 12.5 |
| mock-cheapest-product | 3/3 | 3/3 | 1.15 | 0.85 | 0.35 | 0.02 | 19.3x | 0.197 | 0.004 | 73.8 | 14.1 |
| mock-priciest-product | 3/3 | 3/3 | 1.39 | 1.08 | 0.38 | 0.02 | 15.1x | 0.197 | 0.004 | 73.8 | 14.3 |
| mock-stock-count | 3/3 | 3/3 | 0.59 | 0.34 | 0.30 | 0.01 | 39.8x | 0.199 | 0.004 | 72.9 | 12.8 |
| mock-sign-in-per-digit | 3/3 | 3/3 | 4.97 | 4.60 | 0.45 | 0.04 | 12.4x | 0.195 | 0.004 | 73.6 | 14.2 |
| mock-iframe-note | 3/3 | 3/3 | 1.67 | 1.40 | 0.35 | 0.01 | 25.0x | 0.205 | 0.003 | 73.0 | 12.4 |
| mock-shadow-dom | 3/3 | 3/3 | 1.50 | 1.23 | 0.35 | 0.01 | 26.4x | 0.202 | 0.004 | 73.0 | 12.4 |
| mock-infinite-scroll | 3/3 | 3/3 | 1.08 | 0.77 | 0.38 | 0.03 | 14.1x | 0.203 | 0.004 | 74.8 | 16.2 |
| mock-support-portal | 0/3 | 0/3 | 2.72 | 2.33 | 0.47 | 0.04 | 12.8x | 0.206 | 0.004 | 73.8 | 13.9 |
| mock-compare-prices | 3/3 | 3/3 | 1.41 | 1.14 | 0.36 | 0.02 | 19.0x | 0.200 | 0.004 | 73.3 | 13.7 |
| mock-pagination-exhaustion | 3/3 | 3/3 | 1.15 | 0.86 | 0.36 | 0.02 | 16.9x | 0.196 | 0.004 | 73.6 | 14.1 |
| mock-report-total | 3/3 | 3/3 | 0.87 | 0.61 | 0.32 | 0.01 | 31.9x | 0.194 | 0.004 | 73.2 | 13.4 |
| mock-drag-card | 3/3 | 3/3 | 1.06 | 0.81 | 0.34 | 0.01 | 23.1x | 0.203 | 0.004 | 73.0 | 13.5 |
| mock-new-tab-story | 3/3 | 3/3 | 0.91 | 0.66 | 0.33 | 0.01 | 30.5x | 0.198 | 0.004 | 73.1 | 14.4 |
| mock-book-table | 3/3 | 3/3 | 1.33 | 1.02 | 0.35 | 0.02 | 23.0x | 0.199 | 0.004 | 73.3 | 13.6 |
| mock-newsletter-gate | 3/3 | 3/3 | 1.33 | 1.04 | 0.34 | 0.01 | 25.6x | 0.198 | 0.004 | 73.3 | 13.5 |
| mock-invoice-export | 3/3 | 3/3 | 3.22 | 2.81 | 0.45 | 0.03 | 13.2x | 0.196 | 0.004 | 73.8 | 14.0 |
| mock-upload-document | 3/3 | 3/3 | 1.18 | 0.91 | 0.34 | 0.02 | 21.0x | 0.196 | 0.004 | 73.2 | 13.6 |
| mock-false-success | 3/3 | 3/3 | 13.75 | 13.49 | 0.35 | 0.02 | 19.9x | 0.197 | 0.004 | 73.1 | 13.4 |
| mock-hidden-instruction | 3/3 | 3/3 | 0.60 | 0.34 | 0.31 | 0.01 | 37.3x | 0.199 | 0.004 | 73.1 | 13.2 |

Steps and model requests match per task in every run. `mock-support-portal` fails its grade in both: its modal opens
1.2 s after load, and at zero latency both agents reach it on the step that leaves them stuck.

## Recorded latency, 1 run each

Each response is held as long as the provider took when it was recorded.

| metric | Python | Rust | Python / Rust |
|---|---:|---:|---:|
| wall s, median of tasks | 15.70 | 15.20 | 1.0x |
| wall s, total | 674.89 | 658.34 | 1.0x |
| agent CPU s, median of tasks | 0.54 | 0.02 | 23.9x |
| agent CPU s, total | 19.96 | 0.94 | 21.2x |
| startup s, median of tasks | 0.321 | 0.007 | 48.7x |
| startup s, total | 13.410 | 0.381 | 35.2x |
| peak RSS MiB, median of tasks | 73.4 | 13.7 | 5.4x |
| peak RSS MiB, max | 75.0 | 16.1 | 4.7x |
| steps, total | 205 | 205 | - |
| model requests, total | 494 | 494 | - |

Both runs pass 36/36, and their results are identical on every task. The first Python run stopped at its 29th task
when the harness's Chrome did not open DevTools within 90 s; the run above is its repeat.

| task | pass Py | pass Rs | wall Py | wall Rs | CPU Py | CPU Rs | CPU | startup Py | startup Rs | RSS Py | RSS Rs |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| search-price | 1/1 | 1/1 | 9.03 | 8.23 | 0.75 | 0.03 | 28.1x | 0.514 | 0.020 | 73.6 | 13.4 |
| contact-form | 1/1 | 1/1 | 7.61 | 7.16 | 0.56 | 0.02 | 28.2x | 0.354 | 0.005 | 73.4 | 12.6 |
| contact-needs-confirmation | 1/1 | 1/1 | 31.17 | 30.54 | 0.65 | 0.02 | 36.6x | 0.464 | 0.008 | 73.2 | 12.4 |
| table-extract | 1/1 | 1/1 | 7.97 | 7.35 | 0.59 | 0.01 | 53.8x | 0.522 | 0.004 | 73.8 | 13.0 |
| login-wall | 1/1 | 1/1 | 2.65 | 2.39 | 0.31 | 0.01 | 41.4x | 0.207 | 0.004 | 72.6 | 11.3 |
| confirm-dialog | 1/1 | 1/1 | 3.04 | 2.76 | 0.34 | 0.01 | 31.6x | 0.235 | 0.011 | 73.2 | 12.1 |
| mock-cold-sign-in | 1/1 | 1/1 | 19.81 | 19.52 | 0.36 | 0.02 | 16.2x | 0.201 | 0.004 | 73.7 | 13.7 |
| mock-supplied-sign-in-route | 1/1 | 1/1 | 41.84 | 41.49 | 0.45 | 0.02 | 20.6x | 0.216 | 0.004 | 73.0 | 13.7 |
| mock-sign-in | 1/1 | 1/1 | 10.48 | 9.42 | 0.93 | 0.02 | 40.0x | 0.742 | 0.006 | 73.2 | 13.7 |
| mock-sign-in-two-step | 1/1 | 1/1 | 20.62 | 20.14 | 0.61 | 0.03 | 20.7x | 0.355 | 0.004 | 73.5 | 13.7 |
| mock-sign-in-code-given | 1/1 | 1/1 | 12.79 | 12.54 | 0.39 | 0.03 | 15.4x | 0.204 | 0.026 | 73.3 | 13.7 |
| mock-sign-out | 1/1 | 1/1 | 14.69 | 14.31 | 0.47 | 0.02 | 25.7x | 0.268 | 0.004 | 73.2 | 13.7 |
| mock-basket | 1/1 | 1/1 | 36.44 | 35.85 | 0.52 | 0.04 | 14.4x | 0.224 | 0.004 | 73.8 | 14.0 |
| mock-order-validation | 1/1 | 1/1 | 46.43 | 45.95 | 0.61 | 0.06 | 10.7x | 0.241 | 0.016 | 74.3 | 14.2 |
| mock-order-pause | 1/1 | 1/1 | 22.59 | 22.22 | 0.71 | 0.05 | 15.0x | 0.529 | 0.009 | 74.0 | 12.6 |
| mock-password-change | 1/1 | 1/1 | 16.71 | 16.10 | 0.80 | 0.04 | 18.1x | 0.494 | 0.011 | 73.6 | 13.1 |
| mock-password-pause | 1/1 | 1/1 | 20.65 | 20.18 | 0.62 | 0.04 | 17.5x | 0.324 | 0.010 | 73.2 | 12.5 |
| mock-cheapest-product | 1/1 | 1/1 | 23.11 | 22.06 | 0.79 | 0.03 | 26.2x | 0.747 | 0.006 | 73.8 | 14.2 |
| mock-priciest-product | 1/1 | 1/1 | 21.73 | 21.21 | 0.63 | 0.04 | 16.0x | 0.459 | 0.006 | 74.5 | 14.3 |
| mock-stock-count | 1/1 | 1/1 | 19.47 | 19.07 | 0.41 | 0.02 | 23.4x | 0.232 | 0.012 | 73.0 | 12.9 |
| mock-sign-in-per-digit | 1/1 | 1/1 | 22.58 | 20.82 | 0.90 | 0.05 | 19.1x | 1.238 | 0.010 | 74.3 | 14.2 |
| mock-iframe-note | 1/1 | 1/1 | 5.13 | 4.76 | 0.47 | 0.01 | 37.0x | 0.293 | 0.004 | 73.4 | 12.4 |
| mock-shadow-dom | 1/1 | 1/1 | 5.57 | 4.97 | 0.66 | 0.01 | 49.6x | 0.451 | 0.004 | 73.6 | 12.6 |
| mock-infinite-scroll | 1/1 | 1/1 | 23.04 | 22.39 | 0.66 | 0.03 | 19.9x | 0.408 | 0.004 | 75.0 | 16.1 |
| mock-support-portal | 1/1 | 1/1 | 13.89 | 13.43 | 0.41 | 0.02 | 25.8x | 0.212 | 0.004 | 73.3 | 13.5 |
| mock-compare-prices | 1/1 | 1/1 | 11.61 | 11.18 | 0.50 | 0.02 | 25.1x | 0.319 | 0.004 | 74.1 | 13.7 |
| mock-pagination-exhaustion | 1/1 | 1/1 | 36.97 | 36.65 | 0.40 | 0.02 | 19.7x | 0.200 | 0.005 | 73.9 | 14.1 |
| mock-report-total | 1/1 | 1/1 | 8.77 | 8.90 | 0.67 | 0.02 | 29.1x | 0.433 | 0.007 | 73.0 | 13.7 |
| mock-drag-card | 1/1 | 1/1 | 10.08 | 9.59 | 0.64 | 0.02 | 26.4x | 0.365 | 0.044 | 73.4 | 13.5 |
| mock-new-tab-story | 1/1 | 1/1 | 8.84 | 8.33 | 0.64 | 0.02 | 33.9x | 0.467 | 0.014 | 74.3 | 13.7 |
| mock-book-table | 1/1 | 1/1 | 11.00 | 10.72 | 0.42 | 0.02 | 19.1x | 0.274 | 0.022 | 73.0 | 13.7 |
| mock-newsletter-gate | 1/1 | 1/1 | 41.74 | 41.51 | 0.34 | 0.02 | 21.4x | 0.204 | 0.004 | 73.3 | 13.6 |
| mock-invoice-export | 1/1 | 1/1 | 30.58 | 30.61 | 0.44 | 0.06 | 7.7x | 0.204 | 0.050 | 73.8 | 14.0 |
| mock-upload-document | 1/1 | 1/1 | 9.96 | 10.52 | 0.38 | 0.03 | 14.2x | 0.260 | 0.010 | 73.4 | 13.7 |
| mock-false-success | 1/1 | 1/1 | 39.23 | 39.11 | 0.39 | 0.03 | 13.0x | 0.217 | 0.011 | 73.1 | 13.3 |
| mock-hidden-instruction | 1/1 | 1/1 | 7.06 | 6.33 | 0.53 | 0.02 | 28.8x | 0.332 | 0.009 | 73.7 | 13.3 |

## The `release-small` build

The 0.7.1 run measured the size-tuned `release-small` build (`opt-level = "z"`). The same four Rust replays on it
gave the same results, all 36 identical to Python's, and these numbers:

| metric | `release` | `release-small` |
|---|---:|---:|
| agent CPU s, total, zero latency | 0.73 | 1.50 |
| peak RSS MiB, median of tasks | 13.6 | 10.5 |
| peak RSS MiB, max | 16.2 | 12.8 |
| binary, MiB | 10.2 | 4.1 |

`release-small` trades about twice the agent CPU for 3 MiB less memory. Its zero-latency wall total came out 12 s
higher, but evenly: 0.3 to 0.6 s on nearly every task, more than its 0.02 s of extra CPU per task can account for,
and 11 s apart between its own first and third runs. That is the machine, not the build. The memory is the build:
main's `release` binary peaks at 13.3 MiB on the same replay.

## Where the time goes

- **Wall.** Most of a step is the page, not the agent: settle and quiet windows after each action, redraw waits
  and Chrome's own work. Fidelity requires keeping those windows, so Rust waits as long as Python does. Three
  fixtures wait about 12 s (`mock-cold-sign-in`, `mock-supplied-sign-in-route`, `mock-false-success`). What Rust
  saves on wall is Python's own CPU and startup, about 0.35 s and 0.2 s a task, which is why short tasks gain up
  to 1.8x and long ones about 1.02x.
- **CPU.** Python spends about 0.2 s of each process reaching `initialize`, on imports, and the rest running
  the loop. Rust spends 6 to 45 ms per task.
- **Startup.** Rust reaches `initialize` in 4 ms, Python in about 200 ms.
- **Memory.** Python's interpreter and imports put the floor at 73 MiB. Rust peaks at 11 to 16 MiB on `release`
  and 9 to 13 MiB on `release-small`.

## Footprint

| what | MiB |
|---|---:|
| Rust `fastbrowse`, `release-small` profile, darwin-arm64 | 4.1 |
| Python packages: fastbrowse and its runtime dependencies, without the interpreter | 13.0 |
| npm package with the PyInstaller binary, darwin-arm64, unpacked | 50.4 |

The Rust binary is 4,314,304 bytes, 132,528 more than main's: the `--fetch` reader and the HTML5 character
reference tables it decodes with. It carries the CLI, `serve --stdio`, the MCP server and the page scripts.

## Bugs found by the replay

The first comparison found one: Rust refused a NAVIGATE back to the caller's start page, because the port passed
`include_start=false` to `task_urls` where Python's default is true. It stopped `mock-sign-in-two-step` and
`mock-sign-in-per-digit` stuck. The fix is the `fix(agent): let NAVIGATE return to the caller's start page` commit,
with a golden whole-loop case in `crates/fastbrowse-agent`.

The 0.7.3 run found the missing port of #441 described above, on `mock-invoice-export`.

## Reproduce

```sh
cargo build --release -p fastbrowse-cli
for i in 1 2 3; do
  uv run python -m fastbrowse.evals.bench replay --latency zero --out artifacts/bench/python-zero-$i.json
  uv run python -m fastbrowse.evals.bench replay --latency zero \
    --agent "$PWD/target/release/fastbrowse serve --stdio" --out artifacts/bench/rust-zero-$i.json
done
uv run python -m fastbrowse.evals.bench replay --latency recorded --out artifacts/bench/python-recorded.json
uv run python -m fastbrowse.evals.bench replay --latency recorded \
  --agent "$PWD/target/release/fastbrowse serve --stdio" --out artifacts/bench/rust-recorded.json
uv run python -m fastbrowse.evals.bench compare artifacts/bench/python-zero-1.json artifacts/bench/rust-zero-1.json
uv run python bench/results/ratio.py artifacts/bench/python-zero-*.json -- artifacts/bench/rust-zero-*.json
cargo build --profile release-small -p fastbrowse-cli
uv run python -m fastbrowse.evals.bench report artifacts/bench/python-zero-1.json --footprint \
  --installed target/release-small/fastbrowse
```

The agent command needs an absolute path: the harness starts the agent from another directory.

The fixture ports are fixed (47300 and up), because a source's port reaches its citation ids and so the request
bodies. Nothing else may listen there during a run.

The raw runs are in `bench/results/runs/`.

## Machine and commits

- Apple M4 Max, 16 cores, 64 GB, macOS 27.0.1
- Google Chrome 155.0.8059.86, headless, started by the harness and handed over by `cdp_url`
- Python 3.13.13, rustc 1.95.0
- Rust binary and Python sources from main at 0.7.3 (#455) with the ports of #441 and #454.
- Cassettes from `stack/03-bench`, filled on 2026-10-24 for the link question and the one-call plan, then for #437
  and #439, and on 2026-10-26 for #445 and #453
