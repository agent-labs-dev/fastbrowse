---
name: live-eval
description: "Run fastbrowse builds against Parallax's live sites, compare configs or releases, and find the first wrong step in a failed run. Use for a ship check, a model or setting A/B, a regression question against a release, or triaging why a live task failed."
---

# Live evals

The live harness grades a build on the tasks Parallax defines, with their own truth and checks, and keeps each
run's full trace. Unit tests and the golden fixtures cannot tell whether the agent still browses well; this can.
The scripts are in `scripts/experiments/`; its README says what each one runs.

## Run

1. **Build each tree under test** in its own worktree: `uv sync --all-extras --reinstall-package fastbrowse`. A run
   uses whatever native module that worktree's venv holds, so a stale build measures old code.
2. **Display and keys.** Headed Chrome needs an X display (`Xvfb :99 &`, `DISPLAY=:99`). Read
   `OPENROUTER_API_KEY` from the environment and never print it. Note the spend before and after with
   `GET https://openrouter.ai/api/v1/key` (`data.usage`).
3. **One config per process**, each with its own `--worker-base` (Chrome ports are `9400 + worker`), its settings as
   environment variables, `--repeat 2`, and the task list `ship_check.sh` names. Done: every config's `runs.jsonl`
   has one row per task and repeat.

## Compare

`python scripts/experiments/compare_runs.py DIR [DIR ...]` prints the per-task table, the median time, the model
calls per run by purpose, and every failure in full.

- **Two repeats are noise.** Rerun the tasks a config split on before calling a difference a regression.
- **Against a release**, use the published rows in `fastbrowse-site/data/benchmark-<version>.json` (group
  `internal-fastbrowse`, which carries `llm_calls` and `jev_calls`). For a controlled answer, run that release's
  tag through the same harness.
- **Calls per run** show where a config's time goes. Recovery calls in double figures mean the run looped.

## Triage a failure

`compare_runs.py DIR --task TASK --repeat N` prints the steps, who decided each, their notes and facts, and the
calls in order. Find the **first wrong step**:

1. **The harness first.** Compare `observed` (the final page as graded) with the answer. A grader that cannot see a
   control or the full text fails a correct run.
2. **Jev's pick.** A step whose operation or target is wrong, with the right control offered.
3. **The read.** Facts that cite the right blocks without stating the value, or a list that goes on with no
   continuation, so code never pages.
4. **Recovery and verification.** A loop of recover calls, or a verdict that doubts what the evidence shows.

Name the producer of the wrong value, not the step where the run finally stopped.

## Rules

- **Nothing a grader or trace sees is cut.** The harness reads every control and the whole page text. This is the
  rule AGENTS.md states for models, and it holds for the harness too.
- **Push every experiment.** Commit harness changes and results scripts to the experiments branch and open a PR,
  marked not for merging.
