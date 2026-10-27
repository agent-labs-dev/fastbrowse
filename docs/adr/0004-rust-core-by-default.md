# The Rust core runs by default, and the Python core is retired

[ADR 0001](0001-rust-core-with-in-process-bindings.md) ported the core to Rust one layer at a time, behind a
switch that defaulted to Python. Every layer is now ported. On 0.7.3 the Rust binary matches Python on all 36
fixture tasks, at both latencies and in process, and uses 18x less agent CPU, starts 54x faster and peaks at 5.4x
less memory ([bench/results/rust-vs-python.md](../../bench/results/rust-vs-python.md)). The Rust core becomes the
only core. Both interfaces keep shipping: `fastbrowse` on PyPI and `fastbrowse` on npm.

## Python

`fastbrowse` on PyPI keeps its public surface: `run_task`, `connect_cdp`, `resolve_cdp_port` and the pydantic
models, with the same signatures. What changes is underneath them:

- The wheel is built by maturin, one abi3 wheel per platform, and carries the PyO3 module that is
  `fastbrowse_native` today. The Python sources become the thin layer over it.
- `run_task` builds the Rust `RunTask` directly instead of going through the `serve` params. The arguments the
  protocol has no field for cross in as host callbacks: `jev` and `llm` objects answer the Rust `JevClient` and
  `LLMClient` traits, `check_access` is awaited where Python awaited it, `http` carries the requests the run makes,
  and `config` crosses as data.
- The `fastbrowse` and `fastbrowse-mcp` commands call the Rust CLI through the same module.

## Node

The platform packages carry the Rust binary and the Node addon instead of the PyInstaller build. The SDK already
starts either one, and runs in process with `inProcess`.

## Tooling

The eval, bench and audit harnesses stay Python. They leave the shipped package and drive the Rust core through
`serve --stdio` or the in-process module, as `bench` already does. The parts that call Python internals directly
(`evals/probe.py`, `evals/latency.py`) are retired or rewritten against the protocol.

## Order

1. Callbacks for every `run_task` argument, so no call falls back to Python.
2. The Python wheel built by maturin, with the commands on the Rust CLI.
3. Rust by default: in `run_task`, the commands, the npm packages, CI and the eval workflows. The Python core
   stays reachable with `FASTBROWSE_PYTHON_CORE=1`. This ships as 0.8.0.
4. The Arch package builds the Rust core and becomes one package per architecture.
5. 0.9.0 deletes the Python core, its tests, the PyInstaller build and the `cdp-use` dependency, and with them
   the escape hatch.

## Consequences

- Installing from the sdist needs a Rust toolchain. ADR 0001 named this as the step that is hard to undo.
- A Python change no longer needs a Rust port by hand, since there is one core.
- Between 0.8.0 and 0.9.0 the Python core is a fallback nobody iterates on. A fix lands in Rust only.
