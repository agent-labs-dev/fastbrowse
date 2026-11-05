#!/usr/bin/env python3
"""Turn a campaign's runs.jsonl into results.md, with every run, per-task tables and totals, and gzip its raw output.

python3 bench/head_to_head/report.py NAME
"""

# The table rows and page markup read better whole than wrapped.
# ruff: noqa: E501

import gzip
import json
import shutil
import statistics
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))
from tasks import TASKS, catalog_entry

HERE = Path(__file__).parent
ARM_NAMES = {
    "fastbrowse": "fastbrowse",
    "browser-use": "Browser Use",
    "browser-use-default-effort": "Browser Use, default effort",
    "browser-use-cloud": "Browser Use Cloud, default",
    "bu-ultrafast": "bu-ultrafast",
    "bu-fast": "bu-fast",
}


def load(name: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    out = HERE / "results" / name
    rows = [json.loads(line) for line in (out / "runs.jsonl").read_text().splitlines()]
    return json.loads((out / "environment.json").read_text()), rows


def _median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def _mean(values: list[float]) -> float | None:
    return statistics.fmean(values) if values else None


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """One arm on one task, or on any set of runs: every run counts, passing or not."""
    known = [row for row in rows if row.get("steps") is not None]
    return {
        "runs": len(rows),
        "passed": sum(row["passed"] for row in rows),
        "wall": [row["wall_seconds"] for row in rows],
        "wall_median": _median([row["wall_seconds"] for row in rows]),
        "steps_median": _median([row["steps"] for row in known]),
        "tool_calls_median": _median([len(row.get("tool_calls") or []) for row in known]),
        "model_calls_median": _median([row["model_calls"] for row in rows if row.get("model_calls") is not None]),
        "llm_calls_median": _median([row["llm_calls"] for row in rows if row.get("llm_calls") is not None]),
        "jev_calls_median": _median([row["jev_calls"] for row in rows if row.get("jev_calls") is not None]),
        "llm_input_tokens_median": _median(
            [row["llm_input_tokens"] for row in rows if row.get("llm_input_tokens") is not None]
        ),
        "llm_output_tokens_median": _median(
            [row["llm_output_tokens"] for row in rows if row.get("llm_output_tokens") is not None]
        ),
        "input_tokens_median": _median([row["input_tokens"] for row in rows if row.get("input_tokens") is not None]),
        "output_tokens_median": _median([row["output_tokens"] for row in rows if row.get("output_tokens") is not None]),
        "dollars_mean": _mean([row["dollars"] for row in rows if row.get("dollars") is not None]),
        "dollars_total": sum(row.get("dollars") or 0 for row in rows),
        "unknown_cost_runs": sum(bool(row.get("unknown_cost")) or row.get("dollars") is None for row in rows),
        "session_dollars_mean": _mean(
            [row["session_dollars"] for row in rows if row.get("session_dollars") is not None]
        ),
        "model_seconds_median": _median([row["model_seconds"] for row in rows if row.get("model_seconds") is not None]),
        "cpu_median": _median([row["cpu_seconds"] for row in rows if row.get("cpu_seconds") is not None]),
        "rss_median": _median([row["peak_rss_mib"] for row in rows if row.get("peak_rss_mib") is not None]),
    }


def summaries(rows: list[dict[str, Any]]) -> dict[str, Any]:
    arms = sorted({row["arm"] for row in rows}, key=list(ARM_NAMES).index)
    of = lambda arm, want: summarize([row for row in rows if row["arm"] == arm and want(row)])  # noqa: E731
    return {
        "arms": arms,
        "tasks": {task.id: {arm: of(arm, lambda row, t=task: row["task"] == t.id) for arm in arms} for task in TASKS},
        "short": {arm: of(arm, lambda row: row["length"] == "short") for arm in arms},
        "long": {arm: of(arm, lambda row: row["length"] == "long") for arm in arms},
        "all": {arm: of(arm, lambda row: True) for arm in arms},
    }


def _n(value: float | None, pattern: str = "{:.0f}") -> str:
    return "not measured" if value is None else pattern.format(value)


def _summary_row(label: str, arm: str, s: dict[str, Any]) -> str:
    return (
        f"| {label} | {ARM_NAMES[arm]} | {s['passed']}/{s['runs']} | {_n(s['wall_median'], '{:.1f}')} | "
        f"{_n(s['steps_median'])} | {_n(s['tool_calls_median'])} | {_n(s['llm_calls_median'])} | {_n(s['jev_calls_median'])} | "
        f"{_n(s['llm_input_tokens_median'])} | {_n(s['llm_output_tokens_median'])} | {_n(s['dollars_mean'], '${:.4f}')} |"
    )


HEADER = (
    "| Task | Arm | Passed | Median wall (s) | Median steps | Median tool calls | Median LLM calls | Median Jev calls | "
    "Median text-model tokens in | Median text-model tokens out | Mean cost |\n|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|"
)


def markdown(name: str, env: dict[str, Any], rows: list[dict[str, Any]]) -> str:
    s = summaries(rows)
    lines = [f"# Head-to-head results: {name}", "", "## Setup", "", "```json", json.dumps(env, indent=2), "```", ""]
    for length in ("short", "long"):
        lines += [f"## {length.capitalize()} tasks", "", HEADER]
        for task in TASKS:
            if task.length == length:
                lines += [_summary_row(task.id, arm, s["tasks"][task.id][arm]) for arm in s["arms"]]
        lines += [_summary_row(f"**all {length}**", arm, s[length][arm]) for arm in s["arms"]]
        lines.append("")
    lines += ["## All tasks", "", HEADER]
    lines += [_summary_row("**all six**", arm, s["all"][arm]) for arm in s["arms"]]
    lines += ["", "## Tasks as asked", ""]
    lines += [f"- `{task.id}` ({task.length}): {catalog_entry(task)['task']}" for task in TASKS]
    lines += [
        "",
        "## Every run",
        "",
        "| Run | Preset | Started (UTC) | Passed | Ending | Wall (s) | Steps | Tool calls | LLM | Jev | All tokens in | All tokens out | Cost | Agent CPU (s) | Peak RSS (MiB) | Load before | Load after | Failure |",
        "|:--|:--|:--|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|:--|",
    ]
    for row in rows:
        cost = (
            "not measured"
            if row.get("dollars") is None
            else f"${row['dollars']:.4f}{' (floor)' if row.get('unknown_cost') else ''}"
        )
        tools = len(row["tool_calls"]) if row.get("tool_calls") is not None else "not measured"
        failure = (row.get("error") or row["failure"] or "").replace("|", "\\|")
        lines.append(
            f"| {row['run_id']} | {row.get('preset', 'not hosted')} | {row['started_at']} | {'yes' if row['passed'] else 'no'} | {row['ending']} | {row['wall_seconds']:.1f} | "
            f"{row.get('steps', 'not measured')} | {tools} | {row.get('llm_calls', 'not measured')} | {row.get('jev_calls', 'not measured')} | "
            f"{row.get('input_tokens', 'not measured')} | {row.get('output_tokens', 'not measured')} | {cost} | {_n(row['cpu_seconds'], '{:.1f}')} | {_n(row['peak_rss_mib'])} | "
            f"{_n(row['load_1m_before'], '{}')} | {_n(row['load_1m_after'], '{}')} | {failure} |"
        )
    lines += ["", "## Every answer", ""]
    for row in rows:
        answer = " ".join(str(row.get("answer")).split())
        lines.append(f"- `{row['run_id']}`: {answer}")
    return "\n".join(lines) + "\n"


def plain_punctuation(text: str) -> str:
    """Answers are quoted verbatim, and `scripts/no_slop.py` bans typographic punctuation in tracked files, so it is
    written as numeric entities: Markdown and HTML both show the original character."""
    return text.translate({code: f"&#{code};" for code in (0x2013, 0x2014, 0x2015, 0x2018, 0x2019, 0x201C, 0x201D)})


def pack(raw: Path) -> None:
    """Gzip each run's raw output in place, leaving what is already packed."""
    for file in raw.glob("*/*"):
        if file.suffix != ".gz":
            with file.open("rb") as source, gzip.open(f"{file}.gz", "wb") as target:
                shutil.copyfileobj(source, target)
            file.unlink()


def main() -> None:
    name = sys.argv[1]
    env, rows = load(name)
    path = HERE / "results" / name / "results.md"
    pack(path.parent / "raw")
    path.write_text(plain_punctuation(markdown(name, env, rows)), encoding="utf-8")
    (path.parent / "summary.json").write_text(json.dumps(summaries(rows), indent=2) + "\n", encoding="utf-8")
    print(path)


if __name__ == "__main__":
    main()
