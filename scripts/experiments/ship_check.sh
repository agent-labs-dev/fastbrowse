#!/usr/bin/env bash
# Ship check: the tasks any config or main split on, plus three controls, twice each, on main and the stack top.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
S="$1"
: "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY}"
[ -n "$OPENROUTER_API_KEY" ] || exit 1
TASKS="saucedemo-checkout saucedemo-pause pypi-structured pypi-newer pizza-order quotes-rowling-count dynamic-loading
books-mystery-cheapest quotes-einstein-count quotes-js-page2 crates-serde stretch-books-nonfiction-five-star
stretch-books-young-adult-one-star stretch-quotes-top-authors stretch-date-range-monday stretch-bstack-apple-google
stretch-calendar-first-friday stretch-books-sequential-art-one-star stretch-bstack-apple-samsung
stretch-datepicker-last-sunday hn-top wiki-open practice-login"
GEM="FASTBROWSE_LLM_MODEL=google/gemini-3.8-flash FASTBROWSE_LLM_REASONING=low"
FAST='FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS={"claim_problem_above":1} FASTBROWSE_GROUNDING=off'
run() {
  name=$1; tree=$2; base=$3; shift 3
  D="$S/$name"; mkdir -p "$D"
  echo "$(date +%T) start $name" >> "$S/progress.log"
  (cd "$HOME/.worktrees/$tree" && env "$@" uv run --no-sync --with websockets python "$HERE/live_harness.py" --config "$name" \
    --workers 2 --worker-base $base --only $TASKS --repeat 2 --out "$D/runs.jsonl" --profiles "$D/profiles" > "$D/runs.log" 2>&1)
  echo "$(date +%T) done $name: $(python3 -c "
import json
rows=[json.loads(l) for l in open('$D/runs.jsonl')]
print(sum(bool(r.get('passed')) for r in rows),'/',len(rows),'runs passed')")" >> "$S/progress.log"
}
run sub-main fastbrowse-main 50 FASTBROWSE_UNUSED=1 &
run sub-gem fastbrowse-eval 60 $GEM &
run sub-gem-fast fastbrowse-eval 70 $GEM $FAST &
run sub-ds fastbrowse-eval 80 FASTBROWSE_UNUSED=1 &
run sub-ds-fast fastbrowse-eval 90 $FAST &
wait
echo "$(date +%T) all done sub" >> "$S/progress.log"
