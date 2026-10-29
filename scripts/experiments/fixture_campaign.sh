#!/usr/bin/env bash
# Ablation, all local: fixtures once, each failure rerun once (a pass on rerun is a flake), Sauce Demo x3 on local Chrome.
set -u
cd "$HOME/.worktrees/fastbrowse-eval"
E="$1"
: "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY}"
export SAUCE_PASSWORD=secret_sauce
[ -n "$OPENROUTER_API_KEY" ] || { echo "$(date +%T) no OpenRouter key; gcloud auth expired?" >> "$E/progress.log"; exit 1; }
T="Log in as standard_user, add the Sauce Labs Backpack and the Sauce Labs Bike Light to the cart, and check out as Ada Lovelace, postcode EC1A 1BB. Finish the order and tell me the total."
arm() {
  name=$1; shift
  D="$E/$name"; mkdir -p "$D"
  echo "$(date +%T) start $name" >> "$E/progress.log"
  if [ "$(wc -l < "$D/fixtures.jsonl" 2>/dev/null || echo 0)" -lt 72 ]; then
    env FASTBROWSE_TRACE="$D/fixtures.trace" "$@" uv run --no-sync python -m fastbrowse.evals.runner --repeat 2 --out "$D/fixtures.jsonl" --budget 20 > "$D/fixtures.log" 2>&1
  fi
  failed=$(python3 -c "import json,sys;print(' '.join(sorted({r['task'] for r in map(json.loads,open('$D/fixtures.jsonl')) if not r['passed']})))")
  if [ -n "$failed" ]; then
    env FASTBROWSE_TRACE="$D/rerun.trace" "$@" uv run --no-sync python -m fastbrowse.evals.runner --repeat 1 --only $failed --out "$D/rerun.jsonl" --budget 20 > "$D/rerun.log" 2>&1
  fi
  for i in 1 2 3; do
    t0=$(date +%s.%N)
    env "$@" uv run --no-sync fastbrowse "$T" --start https://www.saucedemo.com/ --secret password=SAUCE_PASSWORD --authorize --json --max-dollars 1 --local --trace "$D/local$i.trace" > "$D/local$i.json" 2> "$D/local$i.log"
    echo "$(date +%s.%N) - $t0" | bc > "$D/local$i.wall"
  done
  echo "$(date +%T) done $name: $(tail -1 "$D/fixtures.log"); rerun: $(tail -1 "$D/rerun.log" 2>/dev/null)" >> "$E/progress.log"
}
G="FASTBROWSE_LLM_MODEL=google/gemini-3.8-flash FASTBROWSE_LLM_REASONING=low"
arm top-ds-all-on
arm top-ds-best           FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS='{"claim_problem_above":1}'
arm top-ds-best-nogr      FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS='{"claim_problem_above":1}' FASTBROWSE_GROUNDING=off
arm top-gem-all-on        $G
arm top-gem-best          $G FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS='{"claim_problem_above":1}'
arm top-gem-best-nogr     $G FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS='{"claim_problem_above":1}' FASTBROWSE_GROUNDING=off
echo "$(date +%T) all done top" >> "$E/progress.log"
