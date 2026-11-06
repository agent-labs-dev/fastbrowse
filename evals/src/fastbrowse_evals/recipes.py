"""Recipe replay against a fresh run: `fastbrowse-evals browser recipes`.

Each task runs once with `--save-recipe`, then again from that recipe with the same inputs, then from it with other
inputs. A run's wall time, model spend, model calls and screenshots come from its result and its trace, and the
steps a recipe carried out from `decided_by`. Replays are worth keeping only when they cost less than the fresh run
and still answer as it did.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

from pydantic import BaseModel, ConfigDict


class RecipeTask(BaseModel):
    """A task whose fills come from named inputs, so one recipe serves `inputs` and `other_inputs` alike."""

    model_config = ConfigDict(extra="forbid", frozen=True)

    id: str
    task: str
    inputs: dict[str, str]
    other_inputs: dict[str, str]
    expect: str
    other_expect: str


# How long a timed-out run gets to stop its browser after SIGTERM before it is killed.
CLEANUP_GRACE = 30.0

TASKS = (
    RecipeTask(
        id="wikipedia-birth-date",
        task=(
            "Use the search box on https://en.wikipedia.org to search for the person named in input person, "
            "open their article and tell me their birth date"
        ),
        inputs={"person": "Ada Lovelace"},
        other_inputs={"person": "Grace Hopper"},
        expect="1815",
        other_expect="1906",
    ),
    RecipeTask(
        id="python-docs-search",
        task=(
            "Use the search box on https://docs.python.org/3/ to search for the words in input term, "
            "open the first result and tell me its page title"
        ),
        inputs={"term": "asyncio"},
        other_inputs={"term": "pathlib"},
        expect="asyncio",
        other_expect="pathlib",
    ),
    RecipeTask(
        id="duckduckgo-first-result",
        task=(
            "Type the words in input query into the search box on https://html.duckduckgo.com/html/, "
            "submit it, open the first result and tell me its page title"
        ),
        inputs={"query": "rust borrow checker"},
        other_inputs={"query": "python asyncio tutorial"},
        expect="borrow",
        other_expect="asyncio",
    ),
)


@dataclass
class Measured:
    task: str
    arm: str
    status: str
    answer: str
    wall: float
    dollars: float
    calls: int
    screenshots: int
    steps: int
    from_recipe: int
    expected: bool | None
    error: str | None = None
    decided_by: list[str] = field(default_factory=list)


def measure(task: str, arm: str, out: Path, expect: str) -> Measured:
    """A finished run's numbers, from its `--json` result and its `--trace` lines."""
    result = json.loads((out / f"{arm}.json").read_text() or "{}")
    calls = screenshots = 0
    for line in (out / f"{arm}.jsonl").read_text().splitlines():
        event = json.loads(line)
        if event.get("event") == "call":
            calls += 1
        elif event.get("event") == "phase" and event.get("phase") == "screenshot":
            screenshots += 1
    steps = result.get("steps") or []
    decided_by = [step.get("decided_by") or "" for step in steps]
    answer = result.get("answer") or ""
    lines = (result.get("cost") or {}).get("lines") or []
    return Measured(
        task=task,
        arm=arm,
        status=result.get("status", "no result"),
        answer=answer,
        wall=float((out / f"{arm}.wall").read_text()),
        dollars=sum(line.get("dollars") or 0.0 for line in lines),
        calls=calls,
        screenshots=screenshots,
        steps=len(steps),
        from_recipe=decided_by.count("recipe"),
        expected=(expect.lower() in answer.lower()) if expect else None,
        error=result.get("error"),
        decided_by=decided_by,
    )


def _terminated(signum: int, frame: object) -> None:
    raise SystemExit(128 + signum)


def run(binary: str, browser: list[str], task: RecipeTask, out: Path, timeout: float) -> list[Measured]:
    recipe = out / "recipe.json"
    arms = (
        ("fresh", task.inputs, ["--save-recipe", str(recipe)], task.expect),
        ("replay", task.inputs, ["--recipe", str(recipe)], task.expect),
        ("replay-other-inputs", task.other_inputs, ["--recipe", str(recipe)], task.other_expect),
    )
    # A recipe or trace left by an earlier benchmark in this folder would be read as this one's.
    for stale in [
        recipe,
        *(out / f"{arm}.{suffix}" for arm, *_ in arms for suffix in ("json", "jsonl", "err", "wall")),
    ]:
        stale.unlink(missing_ok=True)
    measured = []
    # SIGTERM would end the benchmark without unwinding, leaving a running CLI behind: raise instead, as SIGINT does.
    previous = signal.signal(signal.SIGTERM, _terminated)
    try:
        for arm, inputs, recipe_flags, expect in arms:
            if arm != "fresh" and not recipe.exists():
                break
            flags = [flag for name, value in inputs.items() for flag in ("--input", f"{name}={value}")]
            argv = [binary, *browser, "--json", "--trace", str(out / f"{arm}.jsonl"), *flags, *recipe_flags, task.task]
            began = time.monotonic()
            with (out / f"{arm}.json").open("w") as stdout, (out / f"{arm}.err").open("w") as stderr:
                child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL)
                try:
                    child.wait(timeout=timeout)
                except subprocess.TimeoutExpired:
                    pass
                finally:
                    # On a timeout or an interrupted benchmark, SIGTERM lets the CLI stop the browser it owns (a cloud
                    # one bills until stopped).
                    if child.poll() is None:
                        child.terminate()
                        try:
                            child.wait(timeout=CLEANUP_GRACE)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait()
            (out / f"{arm}.wall").write_text(f"{time.monotonic() - began:.1f}")
            (out / f"{arm}.jsonl").touch()
            measured.append(measure(task.id, arm, out, expect))
    finally:
        signal.signal(signal.SIGTERM, previous)
    return measured


def table(rows: list[Measured]) -> str:
    lines = [
        "| Task | Arm | Status | Wall s | Cost $ | Model calls | Screenshots | Steps (from recipe) | Expected |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        expected = "" if row.expected is None else ("yes" if row.expected else "no")
        lines.append(
            f"| {row.task} | {row.arm} | {row.status} | {row.wall:.0f} | {row.dollars:.4f} | {row.calls} | "
            f"{row.screenshots} | {row.steps} ({row.from_recipe}) | {expected} |"
        )
    return "\n".join(lines)


def main(arguments: list[str]) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser recipes", description=__doc__)
    parser.add_argument("out", type=Path, help="directory for each run's result, trace and recipe")
    parser.add_argument("--fastbrowse", default=os.environ.get("FASTBROWSE_BIN", "fastbrowse"))
    parser.add_argument("--task", action="append", help="run only these task ids (repeatable)")
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument(
        "--browser",
        default="--cloud",
        help="the browser flags each run gets, as one string: --browser=--extension (default: --cloud)",
    )
    args = parser.parse_args(arguments)
    rows: list[Measured] = []
    for task in TASKS:
        if args.task and task.id not in args.task:
            continue
        out = args.out / task.id
        out.mkdir(parents=True, exist_ok=True)
        rows.extend(run(args.fastbrowse, args.browser.split(), task, out, args.timeout))
    (args.out / "summary.json").write_text(json.dumps([asdict(row) for row in rows], indent=2) + "\n")
    (args.out / "summary.md").write_text(table(rows) + "\n")
    sys.stdout.write(table(rows) + "\n")
