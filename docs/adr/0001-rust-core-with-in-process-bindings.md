# The core moves to Rust, with in-process bindings for each language

fastbrowse is written in Python and reaches other languages through `fastbrowse serve`, a JSON-RPC process. The
JavaScript SDK starts a PyInstaller binary of 53 MB unpacked, about 250 ms to `--version`. We are porting the
core to Rust and binding it into each host language in-process: PyO3 for Python and napi-rs for Node. Other
languages can use a C ABI or the existing protocol.

Three spikes ran on 2026-10-23 before this decision:

- A Rust binary that launches Chrome, injects the unchanged `snapshot.js` and prints the indexed controls is
  1.3 MB. It reaches the snapshot in 386 ms where Python takes 630 ms on the same fixture. Its output matched
  Python control for control.
- `origins.py` and `citations.py` ported to a Rust crate with PyO3 and napi-rs bindings. The existing Python
  tests pass against it, and the wheel is about 300 KB.
- The CDP session layer in about 640 lines of Rust: one reader task, concurrent calls, out-of-process iframes in
  their own sessions, dialogs and cancellation. It matched Python's output on frames and dialogs.

The browser layer is estimated at 5,000 to 6,000 lines of Rust, with the page scripts reused unchanged.

## Order

The port goes one layer at a time, and the agent keeps running on Python until each layer is ready:

1. Pure helpers such as origins and citations, behind a switch that defaults to Python.
2. The browser layer: CDP connection, sessions, frames, then `page.py`'s actions and settling.
3. Policy, retrieval and verification.
4. The agent loop, then the entry points.

Each layer lands only when the Python tests for it pass against Rust and the fixture eval suites hold their
pass rate. The JSON-RPC protocol stays the contract with the JavaScript SDK until the whole core is in Rust.

## Consequences

- The Python package goes from one pure wheel to an abi3 wheel per platform, and building its sdist needs Rust.
  This is the step that is hard to undo.
- While both languages run side by side, Python callers parse URLs with `urlsplit` and the Rust core follows
  WHATWG ([ADR 0002](0002-urls-follow-whatwg.md)), so the two can disagree about an unusual URL until the
  callers are ported too.
