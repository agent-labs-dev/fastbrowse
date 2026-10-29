#!/usr/bin/env bash
# Live suites (core, dev, heldout, stretch) per config; failures rerun twice so a pass on rerun reads as a flake.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
S="$1"; cd "$HOME/.worktrees/fastbrowse-eval"
: "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY}"
[ -n "$OPENROUTER_API_KEY" ] || exit 1
BEST='FASTBROWSE_READER=jev FASTBROWSE_READ_CONFIDENCE=0.5 FASTBROWSE_THRESHOLDS={"claim_problem_above":1}'
G="FASTBROWSE_LLM_MODEL=google/gemini-3.8-flash FASTBROWSE_LLM_REASONING=low"
run() {
  name=$1; base=$2; shift 2
  D="$S/$name"; mkdir -p "$D"
  echo "$(date +%T) start $name" >> "$S/progress.log"
  env "$@" uv run --no-sync --with websockets python "$HERE/live_harness.py" --config "$name" --workers 2 --worker-base $base --out "$D/runs.jsonl" --profiles "$D/profiles" > "$D/runs.log" 2>&1
  for pass in 1 2; do
    failed=$(python3 -c "
import json
rows=[json.loads(l) for l in open('$D/runs.jsonl')]
last={}
for r in rows: last[r['task']]=r
print(' '.join(sorted(t for t,r in last.items() if not r.get('passed'))))")
    [ -n "$failed" ] || break
    env "$@" uv run --no-sync --with websockets python "$HERE/live_harness.py" --config "$name" --workers 2 --worker-base $base --only $failed --repeat 1 --out "$D/runs.jsonl" --profiles "$D/profiles" >> "$D/runs.log" 2>&1
  done
  echo "$(date +%T) done $name: $(python3 -c "
import json
rows=[json.loads(l) for l in open('$D/runs.jsonl')]
first={};anyp={}
for r in rows: first.setdefault(r['task'],r); anyp[r['task']]=anyp.get(r['task'],False) or bool(r.get('passed'))
print(sum(bool(r.get('passed')) for r in first.values()),'/',len(first),'first try;',sum(anyp.values()),'with reruns')")" >> "$S/progress.log"
}
run top-ds-all-on 0 &
run top-ds-best 10 $BEST &
run top-gem-all-on 20 $G &
run top-gem-best 30 $G $BEST &
wait
echo "$(date +%T) all done live" >> "$S/progress.log"
