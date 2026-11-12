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

# Report how Jev's recorded choices depend on option order; --yes-spend --max-dollars N asks them again reversed.
jev-order-probe *args:
    uv run python -m fastbrowse.evals.order_probe {{args}}

# Validate and publish rows under their recorded release, then regenerate all outputs.
evals-publish rows:
    uv run python -m fastbrowse.evals.versions --publish auto {{quote(rows)}}

# Regenerate the approved site feed; optional arguments include --bump TASK_ID.
evals-docs *args:
    uv run python -m fastbrowse.evals.versions --docs {{args}}

# Check the Rust workspace, build its Python module and Node addon, and run the Python and SDK suites against them.
rust:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace --no-fail-fast
    cargo test --workspace --doc
    # A packaged crate keeps the workspace's version while its code changes; see the same step in ci.yml.
    rm -rf target/debug/.fingerprint/fastbrowse-* "${CARGO_HOME:-$HOME/.cargo}"/registry/src/-*/fastbrowse*
    cargo package --workspace --exclude fastbrowse-py --exclude fastbrowse-node --exclude fastbrowse-probe --locked
    uv sync --all-extras
    uv run --no-sync pytest -q
    cargo build -p fastbrowse-node --release
    cp target/release/libfastbrowse_node.$(test "$(uname)" = Darwin && echo dylib || echo so) crates/fastbrowse-node/fastbrowse.node
    node --test crates/fastbrowse-node/test/core.test.ts
    FASTBROWSE_TEST_ADDON="$PWD/crates/fastbrowse-node/fastbrowse.node" npm run check:sdk

# The extension as the zip a Chrome Web Store upload takes: the same nine files `fastbrowse extension path`
# writes, at the archive's root. Bump `version` in the manifest before uploading a revision.
extension-zip out="dist/fastbrowse-extension.zip":
    #!/usr/bin/env sh
    set -eu
    dest={{quote(out)}}
    case "$dest" in /*) ;; *) dest="$PWD/$dest" ;; esac
    files="manifest.json background.js popup.html popup.js allowed-methods.json icon-16.png icon-32.png icon-48.png icon-128.png"
    mkdir -p "$(dirname "$dest")"
    rm -f "$dest"
    cd crates/fastbrowse-browser/extension
    zip -q -X "$dest" $files
    echo "$dest"
