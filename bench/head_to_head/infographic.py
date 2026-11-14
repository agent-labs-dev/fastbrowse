#!/usr/bin/env python3
"""Draw a campaign as one self-contained HTML file: no network, no external assets, every figure from runs.jsonl.

    python3 bench/head_to_head/infographic.py NAME [--out FILE]

Each measure gets its own panel on its own zero-based scale, one panel per column, so no two scales share an axis. Every
run is a tick on its bar; the bar is the median. A task-level number never replaces the runs: the table at the end lists
them all. `verdict.txt` beside runs.jsonl, when present, is the written verdict shown under the title.
"""

# The table rows and page markup read better whole than wrapped.
# ruff: noqa: E501

import argparse
import html
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))
from report import ARM_NAMES, load, plain_punctuation, summaries, summarize
from tasks import TASKS, catalog_entry

HERE = Path(__file__).parent
MAIN_ARMS = ("fastbrowse", "browser-use", "browser-use-cloud", "bu-ultrafast")
SCOREBOARD_ARMS = (*MAIN_ARMS, "bu-fast")
HOSTED_ARMS = ("browser-use-cloud", "bu-ultrafast", "bu-fast")
SERIES = {
    "fastbrowse": "var(--series-1)",
    "browser-use": "var(--series-2)",
    "browser-use-default-effort": "var(--series-3)",
    "browser-use-cloud": "var(--series-4)",
    "bu-ultrafast": "var(--series-3)",
    "bu-fast": "var(--series-5)",
}

STYLE = """
.viz-root {
  color-scheme: light;
  --page: #f4f4f1; --surface-1: #fcfcfb; --rule: #e4e3de; --track: #ecebe6;
  --text-primary: #0b0b0b; --text-secondary: #52514e; --text-muted: #6d6c67;
  --series-1: #2a78d6; --series-2: #eb6834; --series-3: #1baf7a; --series-4: #8a5fd1; --series-5: #b58a12;
  --good: #0a7a3e; --critical: #c62f2e;
}
@media (prefers-color-scheme: dark) {
  :root:where(:not([data-theme="light"])) .viz-root {
    color-scheme: dark;
    --page: #111110; --surface-1: #1a1a19; --rule: #30302d; --track: #2a2a27;
    --text-primary: #f0efec; --text-secondary: #c3c2b7; --text-muted: #a3a298;
    --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #9b73e0; --series-5: #c79c1f;
    --good: #4cc38a; --critical: #ff8a8a;
  }
}
* { box-sizing: border-box; }
html, body { margin: 0; padding: 0; }
.viz-root {
  background: var(--page); color: var(--text-primary); min-height: 100vh;
  font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  padding: 32px 20px 64px;
}
.wrap { max-width: 1120px; margin: 0 auto; display: grid; grid-template-columns: minmax(0, 1fr); gap: 20px; }
.card { background: var(--surface-1); border: 1px solid var(--rule); border-radius: 12px; padding: 22px 24px; min-width: 0; }
h1 { font-size: 30px; line-height: 1.2; margin: 0 0 8px; letter-spacing: -0.01em; }
h2 { font-size: 19px; margin: 0 0 4px; }
.sub { color: var(--text-secondary); margin: 0; }
.verdict { margin: 14px 0 0; font-size: 17px; }
.verdict p { margin: 0 0 8px; }
.legend { display: flex; flex-wrap: wrap; gap: 6px 18px; margin-top: 14px; color: var(--text-secondary); font-size: 13px; }
.legend span { display: inline-flex; align-items: center; gap: 7px; }
.swatch { width: 12px; height: 12px; border-radius: 3px; display: inline-block; }
.tiles { display: grid; grid-template-columns: repeat(auto-fit, minmax(290px, 1fr)); gap: 12px; }
.tile { background: var(--surface-1); border: 1px solid var(--rule); border-radius: 12px; padding: 14px; min-width: 0; }
.tile .label { font-size: 13px; color: var(--text-secondary); margin-bottom: 8px; }
.tile .pair { display: grid; grid-template-columns: 1fr auto; gap: 2px 10px; align-items: baseline; }
.tile .who { white-space: nowrap; font-size: 12px; color: var(--text-secondary); display: inline-flex; align-items: center; gap: 6px; }
.tile .num { white-space: nowrap; font-size: 19px; font-weight: 650; font-variant-numeric: tabular-nums; text-align: right; }
.tile .foot { font-size: 12px; color: var(--text-muted); margin-top: 8px; }
.panels { display: grid; gap: 22px; margin-top: 16px; }
.task { border-top: 1px solid var(--rule); padding-top: 16px; }
.task h3 { font-size: 15px; margin: 0; }
.task code { font-size: 12px; color: var(--text-muted); }
.task .ask { font-size: 13px; color: var(--text-secondary); margin: 2px 0 12px; }
.metrics { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 14px 24px; }
.metric .name { font-size: 12px; color: var(--text-muted); margin-bottom: 6px; }
.row { display: grid; grid-template-columns: 1fr 86px; align-items: center; gap: 8px; height: 22px; }
.row + .row { margin-top: 3px; }
.plot { position: relative; height: 14px; background: var(--track); border-radius: 0 4px 4px 0; }
.bar { position: absolute; left: 0; top: 0; bottom: 0; border-radius: 0 4px 4px 0; opacity: .85; }
.tick { position: absolute; top: -3px; bottom: -3px; width: 2px; margin-left: -1px; background: var(--text-primary); border-radius: 1px; }
.tick::after { content: ""; position: absolute; left: -6px; right: -6px; top: -3px; bottom: -3px; }
.value { font-size: 13px; font-variant-numeric: tabular-nums; text-align: right; white-space: nowrap; }
.passes { display: flex; align-items: center; gap: 5px; height: 22px; font-size: 13px; }
.passes + .passes { margin-top: 3px; }
.who-mini { width: 168px; white-space: nowrap; color: var(--text-secondary); display: inline-flex; align-items: center; gap: 6px; font-size: 12px; flex: none; }
.mark { font-weight: 700; width: 16px; text-align: center; }
.mark.ok { color: var(--good); }
.mark.no { color: var(--critical); }
.failure { font-size: 12px; color: var(--text-secondary); margin: 8px 0 0; }
.setup { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 10px 24px; margin: 12px 0 0; }
.setup dt { font-size: 12px; color: var(--text-muted); }
.setup dd { margin: 0; overflow-wrap: anywhere; }
ul { margin: 8px 0 0; padding-left: 20px; }
li { margin: 4px 0; }
details summary { cursor: pointer; font-weight: 600; }
.scroll { overflow-x: auto; margin-top: 12px; }
table { border-collapse: collapse; font-size: 12.5px; width: 100%; font-variant-numeric: tabular-nums; }
th, td { padding: 5px 8px; border-bottom: 1px solid var(--rule); text-align: right; white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
td.wrap-cell, th.wrap-cell { text-align: left; white-space: normal; min-width: 280px; }
th { color: var(--text-muted); font-weight: 600; }
#tip { position: fixed; z-index: 10; pointer-events: none; display: none; max-width: 320px; background: var(--text-primary);
  color: var(--surface-1); padding: 6px 9px; border-radius: 6px; font-size: 12px; }
@media print { .viz-root { background: #fff; } details { display: block; } }
"""

SCRIPT = """
const tip = document.getElementById('tip');
document.addEventListener('mousemove', (event) => {
  const target = event.target.closest('[data-tip]');
  if (!target) { tip.style.display = 'none'; return; }
  tip.textContent = target.dataset.tip;
  tip.style.display = 'block';
  const x = Math.min(event.clientX + 12, innerWidth - tip.offsetWidth - 8);
  tip.style.left = x + 'px';
  tip.style.top = (event.clientY + 14) + 'px';
});
"""


def esc(text: object) -> str:
    return html.escape(str(text), quote=True)


def _seconds(value: float) -> str:
    return f"{value:.1f}s"


def _dollars(value: float) -> str:
    return f"${value:.4f}"


def _count(value: float) -> str:
    return f"{value:,.0f}"


def _swatch(arm: str) -> str:
    return f'<i class="swatch" style="background:{SERIES[arm]}"></i>'


def metric(name: str, arms: tuple[str, ...], rows: list[dict[str, Any]], read: Any, show: Any, scale: float) -> str:
    """One measure of one task: per arm a bar to the median, with a tick for each run. Every run counts."""
    out = [f'<div class="metric"><div class="name">{esc(name)}</div>']
    for arm in arms:
        values = [(row, read(row)) for row in rows if row["arm"] == arm]
        known = sorted(value for _, value in values if value is not None)
        median = known[len(known) // 2] if known else None
        if len(known) % 2 == 0 and known:
            median = (known[len(known) // 2 - 1] + known[len(known) // 2]) / 2
        bar = (
            ""
            if median is None
            else f'<div class="bar" style="width:{median / scale * 100:.2f}%;background:{SERIES[arm]}"></div>'
        )
        ticks = "".join(
            f'<div class="tick" style="left:{value / scale * 100:.2f}%" data-tip="{esc(ARM_NAMES[arm])}, repeat {row["repeat"]}: {esc(show(value))}"></div>'
            for row, value in values
            if value is not None
        )
        shown = "not measured" if median is None else show(median)
        out.append(
            f'<div class="row"><div class="plot" data-tip="{esc(ARM_NAMES[arm])}, median: {esc(shown)}">{bar}{ticks}</div><div class="value">{esc(shown)}</div></div>'
        )
    out.append("</div>")
    return "".join(out)


def _present(rows: list[dict[str, Any]], arms: tuple[str, ...] = MAIN_ARMS) -> tuple[str, ...]:
    """The arms a campaign actually has, so a campaign without the hosted runs draws no empty rows."""
    found = {row["arm"] for row in rows}
    return tuple(arm for arm in arms if arm in found)


def task_block(task: Any, rows: list[dict[str, Any]], scales: dict[str, float]) -> str:
    entry = catalog_entry(task)
    arms = _present(rows)
    these = [row for row in rows if row["task"] == task.id and row["arm"] in arms]
    passes = ['<div class="metric"><div class="name">Passed, each repeat</div>']
    for arm in arms:
        runs = sorted((row for row in these if row["arm"] == arm), key=lambda row: row["repeat"])
        marks = "".join(
            f'<span class="mark {"ok" if row["passed"] else "no"}" data-tip="Repeat {row["repeat"]}: {"passed" if row["passed"] else esc(row.get("error") or row["failure"])}">{"✓" if row["passed"] else "✕"}</span>'
            for row in runs
        )
        passed = sum(row["passed"] for row in runs)
        passes.append(
            f'<div class="passes"><span class="who-mini">{_swatch(arm)}{esc(ARM_NAMES[arm])}</span>{marks}<span class="value">&nbsp;{passed} of {len(runs)}</span></div>'
        )
    passes.append("</div>")
    steps = lambda row: row.get("steps")  # noqa: E731
    panels = [
        "".join(passes),
        metric(
            "Wall clock, median and each run",
            arms,
            these,
            lambda row: row["wall_seconds"],
            _seconds,
            scales["wall"],
        ),
        metric(
            "Cost per run, median and each run",
            arms,
            these,
            lambda row: row.get("dollars"),
            _dollars,
            scales["cost"],
        ),
        metric("Model calls, all kinds", arms, these, lambda row: row.get("model_calls"), _count, scales["calls"]),
        metric("Steps, each agent's own unit", arms, these, steps, _count, scales["steps"]),
        metric(
            "Text-model tokens, in + out",
            arms,
            these,
            lambda row: (
                None if row.get("llm_input_tokens") is None else row["llm_input_tokens"] + row["llm_output_tokens"]
            ),
            _count,
            scales["tokens"],
        ),
    ]
    failures = "".join(
        f'<p class="failure">✕ {esc(ARM_NAMES[row["arm"]])}, repeat {row["repeat"]} ({esc(row["ending"])}, {row["wall_seconds"]:.0f}s): {esc(row.get("error") or row["failure"])}</p>'
        for row in these
        if not row["passed"]
    )
    return f'<div class="task"><h3>{esc(task.id)}</h3><p class="ask">{esc(entry["task"])}</p><div class="metrics">{"".join(panels)}</div>{failures}</div>'


def section(length: str, title: str, note: str, rows: list[dict[str, Any]]) -> str:
    these = [row for row in rows if row["length"] == length and row["arm"] in _present(rows)]
    biggest = lambda read: max([value for row in these if (value := read(row)) is not None], default=0) * 1.04 or 1  # noqa: E731
    scales = {
        "wall": biggest(lambda row: row["wall_seconds"]),
        "cost": biggest(lambda row: row.get("dollars")),
        "calls": biggest(lambda row: row.get("model_calls")),
        "steps": biggest(lambda row: row.get("steps")),
        "tokens": biggest(
            lambda row: (
                None if row.get("llm_input_tokens") is None else row["llm_input_tokens"] + row["llm_output_tokens"]
            )
        ),
    }
    blocks = "".join(task_block(task, rows, scales) for task in TASKS if task.length == length)
    return f'<section class="card"><h2>{esc(title)}</h2><p class="sub">{esc(note)}</p><div class="panels">{blocks}</div></section>'


def tile(label: str, s: dict[str, Any], show: Any, foot: str = "") -> str:
    lines = "".join(
        f'<span class="who">{_swatch(arm)}{esc(ARM_NAMES[arm])}</span><span class="num">{esc(show(s[arm]))}</span>'
        for arm in MAIN_ARMS
        if arm in s
    )
    return f'<div class="tile"><div class="label">{esc(label)}</div><div class="pair">{lines}</div><div class="foot">{esc(foot)}</div></div>'


def _not_measured(value: object) -> str:
    return "not measured" if value is None else str(value)


def run_table(rows: list[dict[str, Any]]) -> str:
    head = [
        "Run",
        "Preset",
        "Started (UTC)",
        "Passed",
        "Ending",
        "Wall (s)",
        "Steps",
        "Tool calls",
        "LLM calls",
        "Jev calls",
        "LLM tokens in",
        "LLM tokens out",
        "All tokens in",
        "All tokens out",
        "Cost ($)",
        "Agent CPU (s)",
        "Peak RSS (MiB)",
        "Load before",
        "Load after",
    ]
    body = []
    for row in rows:
        tools = row.get("tool_calls")
        cells = [
            row["run_id"],
            row.get("preset", "not hosted"),
            row["started_at"][11:19],
            "yes" if row["passed"] else "NO",
            row["ending"],
            f"{row['wall_seconds']:.1f}",
            _not_measured(row.get("steps")),
            "not measured" if tools is None else len(tools),
            _not_measured(row.get("llm_calls")),
            _not_measured(row.get("jev_calls")),
            _not_measured(row.get("llm_input_tokens")),
            _not_measured(row.get("llm_output_tokens")),
            _not_measured(row.get("input_tokens")),
            _not_measured(row.get("output_tokens")),
            "not measured" if row.get("dollars") is None else f"{row['dollars']:.6f}",
            "not measured" if row["cpu_seconds"] is None else f"{row['cpu_seconds']:.1f}",
            "not measured" if row["peak_rss_mib"] is None else f"{row['peak_rss_mib']:.0f}",
            _not_measured(row["load_1m_before"]),
            _not_measured(row["load_1m_after"]),
        ]
        body.append("<tr>" + "".join(f"<td>{esc(cell)}</td>" for cell in cells) + "</tr>")
    return f"<table><thead><tr>{''.join(f'<th>{esc(h)}</th>' for h in head)}</tr></thead><tbody>{''.join(body)}</tbody></table>"


def answer_table(rows: list[dict[str, Any]]) -> str:
    body = "".join(
        f'<tr><td>{esc(row["run_id"])}</td><td>{"yes" if row["passed"] else "NO"}</td><td class="wrap-cell">{esc(" ".join(str(row.get("answer")).split()))}</td><td class="wrap-cell">{esc(row.get("error") or row["failure"] or "")}</td></tr>'
        for row in rows
    )
    return f'<table><thead><tr><th>Run</th><th>Passed</th><th class="wrap-cell">Answer, as returned</th><th class="wrap-cell">Why it failed</th></tr></thead><tbody>{body}</tbody></table>'


def sensitivity(rows: list[dict[str, Any]]) -> str:
    arm = "browser-use-default-effort"
    if not any(row["arm"] == arm for row in rows):
        return ""
    lines = []
    slower = dearer = default_passed = default_runs = 0
    for task in TASKS:
        low = summarize([row for row in rows if row["arm"] == "browser-use" and row["task"] == task.id])
        default = summarize([row for row in rows if row["arm"] == arm and row["task"] == task.id])
        slower += default["wall_median"] > low["wall_median"]
        dearer += default["dollars_mean"] > low["dollars_mean"]
        default_passed += default["passed"]
        default_runs += default["runs"]
        cells = [
            task.id,
            f"{low['passed']} of {low['runs']}",
            f"{default['passed']} of {default['runs']}",
            _seconds(low["wall_median"]),
            _seconds(default["wall_median"]),
            _dollars(low["dollars_mean"] or 0),
            _dollars(default["dollars_mean"] or 0),
        ]
        lines.append("<tr>" + "".join(f"<td>{esc(cell)}</td>" for cell in cells) + "</tr>")
    return f"""<section class="card"><h2>Did pinning Browser Use to low effort matter?</h2>
<p class="sub">The same Browser Use without the reasoning field, so at the provider's default effort, run after the head-to-head and not part of it. Low effort was not a handicap: at the default, Browser Use was slower on {slower} of 6 tasks and dearer on {dearer} of 6, and passed {default_passed} of {default_runs}.</p>
<div class="scroll"><table><thead><tr><th>Task</th><th>Passed, low</th><th>Passed, default</th><th>Median wall, low</th><th>Median wall, default</th><th>Mean cost, low</th><th>Mean cost, default</th></tr></thead><tbody>{"".join(lines)}</tbody></table></div></section>"""


def scoreboard(s: dict[str, Any]) -> str:
    """The digest: per task and per arm, passes, median wall clock, and mean model cost, with the fastest clean arm named."""
    arms = [arm for arm in SCOREBOARD_ARMS if arm in s["arms"]]

    def row(label: str, cell: dict[str, Any]) -> str:
        clean = [arm for arm in arms if cell[arm]["passed"] == cell[arm]["runs"]]
        fastest = min(clean, key=lambda arm: cell[arm]["wall_median"]) if clean else None
        cells = [f"<td>{esc(label)}</td>"]
        for arm in arms:
            c = cell[arm]
            text = f"{c['passed']} of {c['runs']}, {_seconds(c['wall_median'])}, {_dollars(c['dollars_mean'] or 0)}"
            cells.append(f"<td>{'<strong>' + esc(text) + '</strong>' if arm == fastest else esc(text)}</td>")
        return "<tr>" + "".join(cells) + "</tr>"

    rows = [row(f"{task.id} ({task.length})", s["tasks"][task.id]) for task in TASKS]
    rows += [row("All short", s["short"]), row("All long", s["long"]), row("All six", s["all"])]
    head = "".join(f"<th>{esc(ARM_NAMES[arm])}</th>" for arm in arms)
    return (
        '<section class="card"><h2>Per task, side by side</h2><p class="sub">Each cell reads runs passed, median wall clock, mean model cost. Bold is the fastest arm that passed every run.</p>'
        f'<div class="scroll"><table><thead><tr><th>Task</th>{head}</tr></thead><tbody>{"".join(rows)}</tbody></table></div></section>'
    )


def hosted_notes(rows: list[dict[str, Any]]) -> str:
    if not any(row["arm"] in HOSTED_ARMS for row in rows):
        return ""
    return """<li><strong>The hosted arms are not on the same machine or the same model.</strong> Browser Use's presets exist only on its cloud, so its agent, browser and network are its own, and it runs GPT-6 Luna where the local arms run Gemini 3.8 Flash. The clock runs from the create request to seeing the run finish, polled every 0.25 s, so it includes the round trip and queueing. fastbrowse has not been run on GPT-6 Luna here.</li>
<li><strong>Hosted cost is model and search charges</strong>, the same kind of figure as the local arms, with the browser bill shown apart. Steps come from the run's event stream, one per model turn, and the final URL is read from the browser over CDP.</li>
<li><strong>The hosted arms were run after the local ones</strong>, on the same day. The checkout's two public credentials are in their prompt, since the API's secret bindings need a vault.</li>
"""


def page(name: str, env: dict[str, Any], rows: list[dict[str, Any]], verdict: str) -> str:
    s = summaries(rows)
    fastbrowse, browser_use = env["fastbrowse"], env["browser_use"]
    tiles = [
        tile("Runs passed, all six tasks", s["all"], lambda arm: f"{arm['passed']} of {arm['runs']}"),
        tile("Median wall clock, short task runs", s["short"], lambda arm: _seconds(arm["wall_median"])),
        tile("Median wall clock, long task runs", s["long"], lambda arm: _seconds(arm["wall_median"])),
        tile(
            "Mean cost per run, all six tasks",
            s["all"],
            lambda arm: _dollars(arm["dollars_mean"] or 0),
            "Model charges only; Chrome ran locally.",
        ),
        *(
            [
                tile(
                    "Mean bill per run, hosted browser included",
                    {arm: s["all"][arm] for arm in HOSTED_ARMS if arm in s["all"]},
                    lambda arm: _dollars(arm["session_dollars_mean"] or 0),
                    "Browser Use's session total, only for the hosted arms.",
                )
            ]
            if "browser-use-cloud" in s["all"]
            else []
        ),
        tile(
            "Total spend, all runs",
            s["all"],
            lambda arm: _dollars(arm["dollars_total"]),
            f"Across {s['all']['fastbrowse']['runs']} runs each.",
        ),
    ]
    paragraphs = "".join(f"<p>{esc(line)}</p>" for line in verdict.splitlines() if line.strip())
    machine = env["machine"]
    setup = {
        "Machine": f"{machine['name']}, {machine['cpu']}, {machine['vcpus']} vCPUs, {machine['memory_gib']} GiB",
        "fastbrowse": f"{fastbrowse['version']}, commit {fastbrowse['commit']}, {fastbrowse['profile']} build",
        "Browser Use": f"{browser_use['version']} (open-source package)",
        "Browser, local arms": env["chrome"],
        "Text model, local arms": f"{env['model']}, {env['reasoning']} reasoning, through OpenRouter",
        **(
            {
                "Hosted arms": "Browser Use Cloud API v4, on their machines and their browser, no managed proxy. Default: gpt-6-luna at high reasoning. bu-ultrafast: GPT-6 Luna, low reasoning, priority tier. bu-fast: GPT-6 Luna, medium reasoning.",
            }
            if any(row["arm"] in HOSTED_ARMS for row in rows)
            else {}
        ),
        "fastbrowse also calls": "Jev, its choice model, counted in its calls and cost",
        "Limits": f"No step or dollar cap; a run past {env['run_seconds_limit']}s is killed",
        "Run": f"{rows[0]['started_at']} to {rows[-1]['started_at']}, one at a time, order alternating",
    }
    cells = "".join(f"<div><dt>{esc(key)}</dt><dd>{esc(value)}</dd></div>" for key, value in setup.items())
    main_rows = [row for row in rows if row["arm"] in MAIN_ARMS]
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>fastbrowse vs Browser Use, {esc(name)}</title><style>{STYLE}</style></head>
<body class="viz-root"><div class="wrap">
<header class="card"><h1>fastbrowse vs Browser Use</h1>
<p class="sub">Six tasks, three repeats each, {esc(name)}. fastbrowse and open-source Browser Use ran on one machine; the Browser Use Cloud arms ran on Browser Use's.</p>
<div class="verdict">{paragraphs}</div>
<div class="legend">{"".join(f"<span>{_swatch(arm)}{esc(ARM_NAMES[arm])}</span>" for arm in _present(rows))}<span><i class="tick" style="position:relative;display:inline-block;top:0;width:2px;height:12px;margin:0"></i>one run</span></div></header>
<div class="tiles">{"".join(tiles)}</div>
{scoreboard(s)}
{section("short", "Short tasks", "One page or one interaction. Bars share a scale within this section, per measure.", rows)}
{section("long", "Long tasks", "Many pages to aggregate, or a multi-page workflow. Bars share a scale within this section, per measure.", rows)}
{sensitivity(rows)}
<section class="card"><h2>What ran</h2><dl class="setup">{cells}</dl></section>
<section class="card"><h2>How to read these numbers</h2><ul>
{hosted_notes(rows)}<li><strong>Steps are not the same unit.</strong> A Browser Use step is one model turn and can hold several actions. A fastbrowse step is one browser action; its reads and checks are extra model calls. Model calls are the closer measure of work, and they are not equal work either: a Jev call is a small, fast choice.</li>
<li><strong>Tokens here are the shared text model only.</strong> Jev's far larger, far cheaper token counts are in the table below. Dollars cover everything each arm was charged.</li>
<li><strong>The text model is pinned, not fastbrowse's default.</strong> Both arms use the same model, so these figures do not describe fastbrowse on its default model.</li>
<li><strong>How Browser Use won the aggregation task.</strong> It wrote one JavaScript call that fetched and tallied every page. fastbrowse has no such tool and clicked through the pages. In the two failed runs it looped for over 100 steps and its verifier refused the answer: one run returned nothing, the other named two of the three authors and said its evidence was partial.</li>
<li><strong>Three repeats show large gaps, not small ones.</strong> The machine is shared and the sites and model provider are live, so a task's numbers can move between campaigns.</li>
<li><strong>Six practice-site tasks are a small sample.</strong> They say little about hard or bot-walled sites.</li>
</ul></section>
<section class="card"><details><summary>Every run, {len(rows)} in all</summary><div class="scroll">{run_table(rows)}</div></details></section>
<section class="card"><details><summary>Every answer, as returned</summary><div class="scroll">{answer_table(main_rows + [row for row in rows if row["arm"] not in MAIN_ARMS])}</div></details></section>
</div><div id="tip" role="status"></div><script>{SCRIPT}</script></body></html>
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("name")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    env, rows = load(args.name)
    folder = HERE / "results" / args.name
    verdict_file = folder / "verdict.txt"
    verdict = verdict_file.read_text(encoding="utf-8") if verdict_file.exists() else ""
    out = args.out or folder / "infographic.html"
    out.write_text(plain_punctuation(page(args.name, env, rows, verdict)), encoding="utf-8")
    print(out)


if __name__ == "__main__":
    main()
