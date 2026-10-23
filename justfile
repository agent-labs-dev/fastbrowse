# Run the local fixture suite (also uses paid model APIs).
evals-local *args:
    uv run python -m fastbrowse.evals.runner {{args}}

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
    FASTBROWSE_RUST_CORE=1 uv run --no-sync pytest -q tests/test_citations.py tests/test_page.py tests/test_safety.py tests/test_retrieval.py tests/test_agent.py tests/browser/test_safety.py
    cargo build -p fastbrowse-node --release
    cp target/release/libfastbrowse_node.$(test "$(uname)" = Darwin && echo dylib || echo so) crates/fastbrowse-node/fastbrowse.node
    node --test crates/fastbrowse-node/test/core.test.ts
