# Rust against Python on the fixture suites

The Rust `fastbrowse` binary and the Python package both ran `serve --stdio` under the benchmark harness
(`src/fastbrowse/evals/bench.py`, described in `bench/README.md`). Each replayed the same committed cassettes in
`bench/cassettes`, so both agents got the same model responses for the same requests.

This run is on fastbrowse 0.7.0. Main's snapshot and settle changes (#397, #406, #407, #409, #410, #414), the link
question in the click classifier (#412), the end of request hedging (#413), the committed answer and browsing-step
gate (#415), the one-call plan with an entry-only shortcut (#417), the sign-in a page must show before a run ends
`needs_login` (#434), and the screenshot gate on visual content, the per-address challenge reload and the verdict cap
(#437) are ported to Rust. The link question and the one-call plan changed what Python asks, so the cassettes were
filled again (`bench fill`) before these runs. The verdict cap changed the verify request, so six tasks were filled
once more for #437 at zero latency and `mock-support-portal` at recorded latency, for $0.017 in all.

## Result

- **Same outputs.** At zero latency every Rust run matches every Python run on all 36 tasks (6 local, 30 mock):
  status, answer, data, error, final URL, citations, grade and every step. At recorded latency they match on all
  36 too.
- **No new requests.** Neither agent asked anything the recording never asked: 0 replay misses in all eight runs.
- **In process too.** `bench replay --agent in-process` at zero latency gives the same result on all 36 tasks for
  the Python core and for the Rust core through `fastbrowse_native`, and both match the `serve` runs.
- **Every run agrees with itself.** The three zero-latency runs of each agent are identical to each other. That
  includes `mock-support-portal`, which both agents finish stuck at zero latency and complete at recorded latency.
- **Fewer calls.** The one-call plan and the entry-only shortcut take a zero-latency replay from 732 model requests
  to 664 in both agents, and a recorded-latency one from 695 to 642.
- **CPU, startup and memory are well past 2x. Wall time is not.** At zero latency Rust uses 11x less agent CPU,
  starts 56x faster and peaks at 7.1x less memory. Wall time is 1.1x better in total, and 1.0x at recorded
  latency, where the model's own response time dominates.
- **Footprint.** The binary is 4.0 MiB, against 13.0 MiB of Python packages (before the interpreter) and the 50.4
  MiB npm package that carries the PyInstaller build.

## Zero latency, 3 runs each

Medians across the 3 runs per task, then the median and total across the 36 tasks.

| metric | Python | Rust | Python / Rust |
|---|---:|---:|---:|
| wall s, median of tasks | 1.82 | 1.56 | 1.2x |
| wall s, total | 110.67 | 100.17 | 1.1x |
| agent CPU s, median of tasks | 0.37 | 0.03 | 12.2x |
| agent CPU s, total | 13.80 | 1.28 | 10.8x |
| startup s, median of tasks | 0.198 | 0.004 | 56.6x |
| startup s, total | 7.136 | 0.127 | 56.1x |
| peak RSS MiB, median of tasks | 74.9 | 10.5 | 7.1x |
| peak RSS MiB, max | 76.6 | 13.5 | 5.7x |
| steps, total | 232 | 232 | - |
| model requests, total | 664 | 664 | - |


The laptop ran nothing else heavy during these runs. Each Python run and each Rust run started a fresh agent per
task against one headless Chrome the harness owned.

| task | pass Py | pass Rs | wall Py | wall Rs | CPU Py | CPU Rs | CPU | startup Py | startup Rs | RSS Py | RSS Rs |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 3/3 | 2.59 | 2.32 | 0.35 | 0.02 | 14.9x | 0.198 | 0.004 | 74.6 | 10.0 |
| contact-form | 3/3 | 3/3 | 2.29 | 2.01 | 0.35 | 0.03 | 11.5x | 0.201 | 0.004 | 74.5 | 9.7 |
| contact-needs-confirmation | 3/3 | 3/3 | 1.97 | 1.70 | 0.35 | 0.03 | 13.8x | 0.198 | 0.003 | 74.4 | 9.8 |
| table-extract | 3/3 | 3/3 | 0.59 | 0.33 | 0.30 | 0.01 | 29.5x | 0.198 | 0.003 | 74.5 | 9.9 |
| login-wall | 3/3 | 3/3 | 0.58 | 0.33 | 0.30 | 0.01 | 37.4x | 0.199 | 0.003 | 74.0 | 9.1 |
| confirm-dialog | 3/3 | 3/3 | 0.87 | 0.61 | 0.32 | 0.01 | 28.1x | 0.199 | 0.004 | 74.2 | 9.5 |
| mock-cold-sign-in | 3/3 | 3/3 | 14.21 | 13.89 | 0.38 | 0.03 | 12.5x | 0.198 | 0.004 | 75.2 | 10.7 |
| mock-supplied-sign-in-route | 3/3 | 3/3 | 14.42 | 14.11 | 0.38 | 0.03 | 11.6x | 0.197 | 0.003 | 75.1 | 10.7 |
| mock-sign-in | 3/3 | 3/3 | 2.03 | 1.73 | 0.39 | 0.04 | 10.5x | 0.198 | 0.004 | 75.0 | 10.7 |
| mock-sign-in-two-step | 3/3 | 3/3 | 4.74 | 4.37 | 0.53 | 0.08 | 6.6x | 0.196 | 0.004 | 76.0 | 11.4 |
| mock-sign-in-code-given | 3/3 | 3/3 | 2.69 | 2.40 | 0.41 | 0.04 | 9.8x | 0.198 | 0.003 | 75.1 | 10.7 |
| mock-sign-out | 3/3 | 3/3 | 2.03 | 1.72 | 0.38 | 0.03 | 11.1x | 0.197 | 0.004 | 74.6 | 10.4 |
| mock-basket | 3/3 | 3/3 | 3.55 | 3.21 | 0.46 | 0.06 | 7.5x | 0.199 | 0.004 | 75.0 | 10.9 |
| mock-order-validation | 3/3 | 3/3 | 4.82 | 4.44 | 0.53 | 0.08 | 6.5x | 0.197 | 0.004 | 75.5 | 11.1 |
| mock-order-pause | 3/3 | 3/3 | 3.66 | 3.37 | 0.43 | 0.05 | 8.4x | 0.197 | 0.004 | 74.7 | 10.1 |
| mock-password-change | 3/3 | 3/3 | 3.09 | 2.78 | 0.42 | 0.05 | 8.2x | 0.198 | 0.003 | 74.7 | 10.6 |
| mock-password-pause | 3/3 | 3/3 | 2.81 | 2.52 | 0.39 | 0.04 | 10.2x | 0.197 | 0.004 | 74.8 | 10.0 |
| mock-cheapest-product | 3/3 | 3/3 | 1.18 | 0.88 | 0.37 | 0.04 | 10.7x | 0.198 | 0.004 | 75.6 | 11.5 |
| mock-priciest-product | 3/3 | 3/3 | 1.42 | 1.11 | 0.40 | 0.04 | 8.8x | 0.198 | 0.004 | 75.7 | 11.9 |
| mock-stock-count | 3/3 | 3/3 | 0.59 | 0.34 | 0.30 | 0.01 | 31.5x | 0.197 | 0.004 | 74.4 | 9.8 |
| mock-sign-in-per-digit | 3/3 | 3/3 | 6.50 | 6.16 | 0.55 | 0.09 | 5.9x | 0.197 | 0.004 | 76.0 | 11.3 |
| mock-iframe-note | 3/3 | 3/3 | 1.68 | 1.41 | 0.34 | 0.02 | 14.9x | 0.200 | 0.004 | 74.5 | 9.7 |
| mock-shadow-dom | 3/3 | 3/3 | 1.50 | 1.23 | 0.34 | 0.02 | 16.2x | 0.199 | 0.003 | 74.6 | 9.8 |
| mock-infinite-scroll | 3/3 | 3/3 | 1.11 | 0.81 | 0.39 | 0.05 | 7.1x | 0.198 | 0.004 | 76.6 | 13.5 |
| mock-support-portal | 0/3 | 0/3 | 2.75 | 2.40 | 0.46 | 0.06 | 7.7x | 0.197 | 0.004 | 75.5 | 10.7 |
| mock-compare-prices | 3/3 | 3/3 | 1.46 | 1.16 | 0.38 | 0.03 | 12.6x | 0.200 | 0.004 | 75.0 | 10.4 |
| mock-pagination-exhaustion | 3/3 | 3/3 | 1.18 | 0.87 | 0.37 | 0.03 | 10.7x | 0.197 | 0.004 | 75.4 | 11.5 |
| mock-report-total | 3/3 | 3/3 | 0.88 | 0.61 | 0.33 | 0.01 | 23.5x | 0.199 | 0.003 | 74.9 | 10.4 |
| mock-drag-card | 3/3 | 3/3 | 1.10 | 0.84 | 0.35 | 0.02 | 15.5x | 0.199 | 0.004 | 74.9 | 10.5 |
| mock-new-tab-story | 3/3 | 3/3 | 0.94 | 0.75 | 0.34 | 0.01 | 24.6x | 0.200 | 0.004 | 74.9 | 11.6 |
| mock-book-table | 3/3 | 3/3 | 1.33 | 1.04 | 0.36 | 0.02 | 14.7x | 0.199 | 0.004 | 74.9 | 10.5 |
| mock-newsletter-gate | 3/3 | 3/3 | 1.30 | 1.03 | 0.35 | 0.02 | 15.2x | 0.200 | 0.004 | 74.5 | 10.2 |
| mock-invoice-export | 3/3 | 3/3 | 3.28 | 2.94 | 0.47 | 0.06 | 8.0x | 0.200 | 0.004 | 75.4 | 11.0 |
| mock-upload-document | 3/3 | 3/3 | 1.20 | 0.93 | 0.35 | 0.02 | 14.9x | 0.197 | 0.004 | 75.0 | 10.7 |
| mock-false-success | 3/3 | 3/3 | 13.75 | 13.48 | 0.36 | 0.03 | 11.8x | 0.199 | 0.003 | 74.9 | 10.3 |
| mock-hidden-instruction | 3/3 | 3/3 | 0.61 | 0.34 | 0.32 | 0.01 | 25.4x | 0.200 | 0.004 | 75.0 | 10.3 |

Steps and model requests match per task in every run. `mock-support-portal` fails its grade in both: its modal opens
1.2 s after load, and at zero latency both agents reach it on the step that leaves them stuck.
`mock-supplied-sign-in-route` takes about 12 s more in both than in the last comparison, and is most of the rise
in total wall. It starts at `/entry`, a page inside the site, so since #417 neither agent asks for a shortcut to
`/login`: the run loads `/entry`, which holds about 12 s as the cold-start page does, then navigates to `/login`
itself, one step more.

## Recorded latency, 1 run each

Each response is held as long as the provider took when it was recorded.

| metric | Python | Rust | Python / Rust |
|---|---:|---:|---:|
| wall s, median of tasks | 19.92 | 19.64 | 1.0x |
| wall s, total | 916.11 | 904.88 | 1.0x |
| agent CPU s, median of tasks | 0.43 | 0.04 | 10.1x |
| agent CPU s, total | 16.69 | 1.94 | 8.6x |
| startup s, median of tasks | 0.191 | 0.004 | 54.6x |
| startup s, total | 6.897 | 0.128 | 53.7x |
| peak RSS MiB, median of tasks | 75.1 | 10.7 | 7.0x |
| peak RSS MiB, max | 76.5 | 13.6 | 5.6x |
| steps, total | 218 | 218 | - |
| model requests, total | 642 | 642 | - |


Both runs pass 36/36, and their results are identical on every task. Request counts match too: with hedging gone
from both clients, no second request races a slow one, so the two counts no longer drift with timing.

## Where the time goes

- **Wall.** Most of a step is the page, not the agent: settle and quiet windows after each action, redraw waits
  and Chrome's own work. Fidelity requires keeping those windows, so Rust waits as long as Python does. Three
  fixtures wait about 12 s (`mock-cold-sign-in`, `mock-supplied-sign-in-route`, `mock-false-success`). Both agents
  settle on Chrome's lifecycle events and wait for a redraw inside the page, so neither pays a fixed poll there.
  What Rust saves on wall is Python's own CPU and startup, about 0.35 s and 0.2 s a task, which is why short tasks
  gain up to 1.8x and long ones about 1.02x.
- **CPU.** Python spends about 0.2 s of each process reaching `initialize`, on imports, and the rest running
  the loop. Rust spends 10 to 100 ms per task.
- **Startup.** Rust reaches `initialize` in 4 ms, Python in about 200 ms.
- **Memory.** Python's interpreter and imports put the floor at 74 MiB. Rust peaks at 9 to 14 MiB.

## Footprint

| what | MiB |
|---|---:|
| Rust `fastbrowse`, `release-small` profile, darwin-arm64 | 4.0 |
| Python packages: fastbrowse and its runtime dependencies, without the interpreter | 13.0 |
| npm package with the PyInstaller binary, darwin-arm64, unpacked | 50.4 |

The Rust binary is 4,231,904 bytes. It carries the CLI, `serve --stdio`, the MCP server and the page scripts.

## Bugs found by the replay

The first comparison found one: Rust refused a NAVIGATE back to the caller's start page, because the port passed
`include_start=false` to `task_urls` where Python's default is true. It stopped `mock-sign-in-two-step` and
`mock-sign-in-per-digit` stuck. The fix is the `fix(agent): let NAVIGATE return to the caller's start page` commit,
with a golden whole-loop case in `crates/fastbrowse-agent`. Later runs found none.

## Reproduce

```sh
cargo build --profile release-small -p fastbrowse-cli
for i in 1 2 3; do
  uv run python -m fastbrowse.evals.bench replay --latency zero --out artifacts/bench/python-zero-$i.json
  uv run python -m fastbrowse.evals.bench replay --latency zero \
    --agent "$PWD/target/release-small/fastbrowse serve --stdio" --out artifacts/bench/rust-zero-$i.json
done
uv run python -m fastbrowse.evals.bench replay --latency recorded --out artifacts/bench/python-recorded.json
uv run python -m fastbrowse.evals.bench replay --latency recorded \
  --agent "$PWD/target/release-small/fastbrowse serve --stdio" --out artifacts/bench/rust-recorded.json
uv run python -m fastbrowse.evals.bench compare artifacts/bench/python-zero-1.json artifacts/bench/rust-zero-1.json
uv run python bench/results/ratio.py artifacts/bench/python-zero-*.json -- artifacts/bench/rust-zero-*.json
uv run python -m fastbrowse.evals.bench report artifacts/bench/python-zero-1.json --footprint \
  --installed target/release-small/fastbrowse
```

The fixture ports are fixed (47300 and up), because a source's port reaches its citation ids and so the request
bodies. Nothing else may listen there during a run.

The raw runs are in `bench/results/runs/`.

## Machine and commits

- Apple M4 Max, 16 cores, 64 GB, macOS 27.0.1
- Google Chrome 155.0.8059.86, headless, started by the harness and handed over by `cdp_url`
- Python 3.13.13, rustc 1.95.0
- Rust binary built from `fix/rust-port-434-437` on fastbrowse 0.7.0, with main's Python sources at #437. The
  Python runs used the same Python sources.
- Cassettes from `stack/03-bench`, filled on 2026-10-24 for the link question and the one-call plan, then for #437
