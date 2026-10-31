# Architecture

The core is Rust, in [crates/](../../crates) ([ADR 0004](../../docs/adr/0004-rust-core-by-default.md)), and the `fastbrowse`
wheel carries it as `fastbrowse._native`. The Python package is the thin layer over it:
[native.py](../../src/fastbrowse/native.py) hands `run_task`, `connect_cdp` and the commands to it, and the pydantic models
in `src/fastbrowse` are the shapes a caller passes and gets back. A behaviour change lands in the crates.

The run loop is [crates/fastbrowse-agent/src/agent](../../crates/fastbrowse-agent/src/agent), and everything else is a seam it calls.

- **[fastbrowse](../../crates/fastbrowse/src/lib.rs)** is the crate on crates.io a Rust caller depends on: `run_task` and the
  result's types, re-exported. The crates under it are published at the same version so it can be.
- **[fastbrowse-run](../../crates/fastbrowse-run/src/lib.rs)** opens the browser and builds the Jev and LLM clients from the
  environment, then hands control to the loop. Python's `run_task(...)` is this, in process. The browser is one of
  three, in this order: one the caller hands over (`cdp_url` or `cdp_port`, neither started nor stopped here; `attach`
  drives a window already open instead of a new tab, and `connect_cdp` yields that page without the agent), a Browser
  Use Cloud browser (`browser_api_key`), or local Chrome. `start` is optional; without one the first address is
  proposed from the task.
- **[fastbrowse-browser](../../crates/fastbrowse-browser/src)** indexes the page. [browser/snapshot.js](../../crates/fastbrowse-browser/js/snapshot.js) runs in the page and returns the controls
  with what tells them apart (role, label, the card or row that disambiguates twins, whether a field blocks
  its form); [browser/capture.js](../../crates/fastbrowse-browser/js/capture.js) returns the text as source blocks with stable spans, so a reader can cite them.
- **[policy.rs](../../crates/fastbrowse-agent/src/policy.rs)** batches operation and target choices and applicable wall checks.
  Large target sets use a further choice inside a group. [decision.rs](../../crates/fastbrowse-core/src/decision.rs) is the client's shape;
  [system_one.rs](../../crates/fastbrowse-clients/src/system_one.rs) serves the System One API: OpenRouter when keyed, the recommended route for Jev and the LLM, otherwise direct TypeSafe, or Telnyx, Ollama or another host when chosen; [vercel.rs](../../crates/fastbrowse-clients/src/vercel.rs) serves the
  Vercel AI Gateway, which is also the Jev primary when no other Jev key is set. [environment.rs](../../crates/fastbrowse-clients/src/environment.rs) selects routes; [failover.rs](../../crates/fastbrowse-clients/src/failover.rs) switches after retries.
- **[retrieval](../../crates/fastbrowse-agent/src/retrieval)** is Jev's read: one choice over quoted spans cut from the capture, and a composed answer the LLM writes from the notes when Jev asks for one. [memory.rs](../../crates/fastbrowse-agent/src/memory.rs) holds notes with citation ids, and [citations.rs](../../crates/fastbrowse-core/src/citations.rs) builds deep links.
- **[verification](../../crates/fastbrowse-agent/src/verification)** fills a caller's `output_schema` from spans Jev picks. A run ends when Jev picks DONE.
- **[safety.rs](../../crates/fastbrowse-core/src/safety.rs)** owns secrets and irreversible actions. **[effects.rs](../../crates/fastbrowse-core/src/effects.rs)** says what an action actually did,
  which is how a no-op is told from progress. **[telemetry.rs](../../crates/fastbrowse-core/src/telemetry.rs)** is the ledger: steps, calls, dollars.
- **[cli.rs](../../crates/fastbrowse-cli/src/cli.rs)**, **[mcp](../../crates/fastbrowse-cli/src/mcp)**, **[serve.rs](../../crates/fastbrowse-cli/src/serve.rs)** and **`run_task`** are the four entry points; [options.rs](../../crates/fastbrowse-cli/src/options.rs) holds the rules
  they share, so a flag means the same thing in all of them. `serve` is `fastbrowse serve --stdio`: it calls
  `run_task` for another process over JSON-RPC, one run at a time, and adds no agent logic. Its messages are the
  types in [protocol.rs](../../crates/fastbrowse-core/src/protocol.rs), written as JSON Schema to [protocol/schema.json](../../protocol/schema.json); [output_schema.rs](../../crates/fastbrowse-core/src/output_schema.rs) turns a caller's JSON Schema into an `output_schema`.
- **[go](../../go)**, **[java](../../java)** and **[dotnet](../../dotnet)** are the Go, Java (`dev.agentlabs:fastbrowse`) and .NET
  (`Fastbrowse`) clients. Each starts `serve` as the SDK does, with types generated from `protocol/schema.json`
  (`go generate`, [java_types.py](../../scripts/java_types.py), [dotnet_types.py](../../scripts/dotnet_types.py)). The release pins
  the Go module's binary archives by SHA-256 ([go_binaries.py](../../scripts/go_binaries.py)), ships Java's in
  `natives-<platform>` jars, and packs .NET's under `runtimes/<rid>/native`.
- **[packages/sdk](../../packages/sdk)** is the TypeScript SDK, `fastbrowse` on npm. It starts `serve` and knows the agent by
  the protocol alone; [its types](../../packages/sdk/src/protocol.ts) are generated from `protocol/schema.json` (`npm run generate:sdk`).
  What it starts is the Rust `fastbrowse` binary ([crates/fastbrowse-cli](../../crates/fastbrowse-cli)), with the Node addon beside it
  for `inProcess`, shipped as one `@fastbrowse/<os>-<arch>` npm package per platform.

## The Rust and Python seam

Python sees a Rust value only as serde serializes it, so a field marked `#[serde(skip)]` never reaches Python or
the JSON a caller reads. `RunResult.final_frame` is one: the final image travels beside the result, not in it,
and was once lost because nothing tested that it crossed. A change to a type that crosses the binding or the
protocol tests the value on the far side of the seam, not only in the crate that builds it.
