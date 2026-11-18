# The checks CI's `gate` job runs, in its order; `just rust` covers the crates and the SDK checks run in CI.
gate base="origin/main":
    uv run ruff check .
    uv run ruff format --check .
    uv run ty check
    uv run python scripts/changelog.py --check "$(uv version --short)"
    uv run python scripts/npm_versions.py
    uv run python scripts/no_slop.py
    uv run python scripts/docs_sync.py
    uv run vale sync
    uv run vale README.md CHANGELOG.md AGENTS.md CONTRIBUTING.md docs skills src scripts tests
    uv run python -m fastbrowse.evals.storage --manifest docs/results/evidence.manifest.json --check
    uv run pytest -q
    uv run python -m fastbrowse.evals.benchmarks --check-diff {{quote(base)}}
    uv run python -m fastbrowse.evals.publication --check-diff {{quote(base)}}

# Lint, types and prose on the files changed against `base`, the whole-tree text checks and the changed tests.
gate-changed base="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail
    mapfile -t files < <({ git diff --name-only --diff-filter=d "$(git merge-base {{quote(base)}} HEAD)"; git ls-files --others --exclude-standard; } | sort -u)
    py=(); typed=(); prose=(); tests=()
    for f in "${files[@]}"; do
        case "$f" in evals/*) continue;; esac  # its own project, with its own CI job
        case "$f" in *.py) py+=("$f");; esac
        case "$f" in src/*.py|tests/*.py|benchmarks/*.py) typed+=("$f");; esac
        case "$f" in
            README.md|CHANGELOG.md|AGENTS.md|CONTRIBUTING.md) prose+=("$f");;
            docs/*.md|skills/*.md|src/*.py|scripts/*.py|tests/*.py) prose+=("$f");;
        esac
        case "$f" in tests/*test_*.py) tests+=("$f");; esac
    done
    if (( ${#py[@]} )); then uv run ruff check --force-exclude "${py[@]}"; uv run ruff format --check --force-exclude "${py[@]}"; fi
    if (( ${#typed[@]} )); then uv run ty check "${typed[@]}"; fi
    uv run python scripts/no_slop.py
    uv run python scripts/docs_sync.py
    if (( ${#prose[@]} )); then uv run vale sync >/dev/null; uv run vale "${prose[@]}"; fi
    if (( ${#tests[@]} )); then uv run pytest -q "${tests[@]}"; fi

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

# The Store package omits the development key while checking that it identifies the existing item.
# Bump `version` in the manifest before uploading a revision.
extension-zip out="dist/fastbrowse-extension.zip":
    python3 scripts/extension_store.py build --package {{quote(out)}}
