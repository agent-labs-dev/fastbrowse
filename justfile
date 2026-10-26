# Run the local fixture suite (also uses paid model APIs).
evals-local *args:
    uv run python -m fastbrowse.evals.runner {{args}}

# Benchmark any `fastbrowse serve --stdio` on the fixture suites with recorded model traffic (bench/README.md).
bench *args:
    uv run python -m fastbrowse.evals.bench {{args}}

# Replay every cassette three times at zero latency and once at recorded latency, then report.
bench-baseline out="artifacts/bench/baseline" agent=".venv/bin/fastbrowse serve --stdio":
    for i in 1 2 3; do uv run python -m fastbrowse.evals.bench replay --agent {{quote(agent)}} --out {{out}}/zero-$i.json; done
    uv run python -m fastbrowse.evals.bench replay --agent {{quote(agent)}} --latency recorded --out {{out}}/recorded.json
    uv run python -m fastbrowse.evals.bench report {{out}}/zero-*.json {{out}}/recorded.json --footprint --markdown {{out}}/report.md --json {{out}}/summary.json

# Validate and publish rows under their recorded release, then regenerate all outputs.
evals-publish rows:
    uv run python -m fastbrowse.evals.versions --publish auto {{quote(rows)}}

# Regenerate the approved site feed; optional arguments include --bump TASK_ID.
evals-docs *args:
    uv run python -m fastbrowse.evals.versions --docs {{args}}

# Check the Rust workspace, install its Python module and Node addon, and run the suites that cover it.
rust:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    rm -rf target/wheels
    uvx maturin==1.15.0 build --release -m crates/fastbrowse-py/Cargo.toml -i .venv/bin/python --out target/wheels
    uv pip install --reinstall target/wheels/fastbrowse_native-*.whl
    FASTBROWSE_RUST_CORE=1 uv run --no-sync pytest -q tests/test_citations.py tests/test_page.py tests/test_safety.py tests/test_retrieval.py tests/test_agent.py tests/test_native.py tests/browser/test_safety.py tests/browser/test_native_parity.py tests/browser/test_native_recording.py tests/browser/test_native_connect.py
    cargo build -p fastbrowse-node --release
    cp target/release/libfastbrowse_node.$(test "$(uname)" = Darwin && echo dylib || echo so) crates/fastbrowse-node/fastbrowse.node
    node --test crates/fastbrowse-node/test/core.test.ts
    FASTBROWSE_TEST_ADDON="$PWD/crates/fastbrowse-node/fastbrowse.node" npm run check:sdk
