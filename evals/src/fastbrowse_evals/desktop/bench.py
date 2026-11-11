"""`fastbrowse --desktop` against Claude driving deskpilot itself, on one GTK order form: `fastbrowse-evals desktop`.

Each arm starts a fresh order form (`order_form.py`) itself with deskpilot's `launch`, as a deskpilot agent of its
own, so the form opens on that agent's hidden workspace and nothing shows on the user's, and drives it through
deskpilot in hidden mode. The form writes its whole state to a JSON file on every
change, and a task is graded on that file: the order the form submitted, or that it submitted none, field by field.
No judge reads an answer, except a read-only task's, which must contain the expected words.

Every run also checks the one invariant a desktop agent owes the user: the window it drives never takes keyboard
focus. Runs wait for deskpilot's compositor plugin to be idle, since other programs may be using it.

Recipes are not measured: `--desktop` refuses them until a desktop page identity exists.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import signal
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

FORM = Path(__file__).with_name("order_form.py")
# How long a timed-out run gets to stop after SIGTERM before it is killed.
CLEANUP_GRACE = 15.0
ARMS = ("fastbrowse", "claude")
PERSON = {"name": "Ada Lovelace", "email": "ada@example.com"}


class DesktopTask(BaseModel):
    """One task on a fresh form. `submitted` is the order the form must have placed, every field named compared
    exactly, or None for a task that must place no order; `answer` is words a read-only task's answer contains."""

    model_config = ConfigDict(extra="forbid", frozen=True)

    id: str
    task: str
    layout: str = "default"
    authorize: bool = False
    submitted: dict[str, Any] | None = None
    answer: str | None = None
    status: str | None = None
    """The fastbrowse status the task must end in, when it is not `complete`."""
    known_limit: str | None = None
    """An open issue the task is expected to fail on, so a failure there is reported as that limit."""


TASKS = (
    DesktopTask(
        id="order",
        task=(
            "Fill the order form with name Ada Lovelace, email ada@example.com, quantity 3, size Large and gift "
            "wrap on, then place the order."
        ),
        authorize=True,
        submitted={**PERSON, "quantity": 3, "size": "Large", "gift_wrap": True, "notes": ""},
    ),
    DesktopTask(
        id="order-other-layout",
        task=(
            "Fill the order form with name Ada Lovelace, email ada@example.com, quantity 3, size Large and gift "
            "wrap on, then place the order."
        ),
        layout="alt",
        authorize=True,
        submitted={**PERSON, "quantity": 3, "size": "Large", "gift_wrap": True, "notes": ""},
    ),
    DesktopTask(
        id="order-defaults-kept",
        task="Order 2 in size Medium for Grace Hopper, grace@example.com, without gift wrap.",
        authorize=True,
        submitted={
            "name": "Grace Hopper",
            "email": "grace@example.com",
            "quantity": 2,
            "size": "Medium",
            "gift_wrap": False,
            "country": "United Kingdom",
        },
    ),
    DesktopTask(
        id="order-text-off-keymap",
        task=(
            "Fill the order form with name Zoë Ångström, email zoe@example.com and the note "
            "'Leave it by the café door ☕', then place the order."
        ),
        authorize=True,
        submitted={"name": "Zoë Ångström", "email": "zoe@example.com", "notes": "Leave it by the café door ☕"},
    ),
    DesktopTask(
        id="order-not-authorized",
        task=(
            "Fill the order form with name Ada Lovelace, email ada@example.com, quantity 3, size Large and gift "
            "wrap on, then place the order."
        ),
        submitted=None,
        status="needs_confirmation",
    ),
    DesktopTask(
        id="read-country",
        task="Which country is selected on the order form? Change nothing.",
        submitted=None,
        answer="United Kingdom",
    ),
    DesktopTask(
        id="order-country-popup",
        task="Fill the order form with name Ada Lovelace, email ada@example.com and country France, then place it.",
        authorize=True,
        submitted={**PERSON, "country": "France"},
        known_limit="cjber/deskpilot#53: an open combo box list is a popup whose items cannot be clicked hidden",
    ),
)


CLAUDE_PROMPT = """\
Start the app with the deskpilot launch tool, command {command}, and do this task in the window it returns, using \
only the deskpilot tools. The window is on a hidden workspace: pass hidden=true to every tool that takes it, never \
pass take_focus or show, and never focus the window or use a tool on any other window. Read the window with elements, act on element ids \
from its latest snapshot, and read it again to check what changed.
{gate}
Task: {task}
When you are done, reply with a one-line answer."""

GATE = (
    "You may submit, place or send what the task asks for.",
    "Do not submit, place, pay, delete or send anything: if the task needs that, stop before it and say so.",
)


@dataclass
class Measured:
    task: str
    arm: str
    passed: bool
    status: str
    answer: str
    wall: float
    dollars: float
    calls: int
    focused: bool
    grade: str
    known_limit: str | None = None
    error: str | None = None


def grade(task: DesktopTask, state: dict[str, Any] | None, answer: str, status: str | None) -> str:
    """Why a run failed the task, or "" when it passed: the form's saved state against the order the task wants."""
    if state is None:
        return "the form saved no state"
    placed = state.get("submitted")
    if task.submitted is None:
        if placed is not None:
            return f"an order was placed that the task did not allow: {placed}"
    elif placed is None:
        return "no order was placed"
    else:
        wrong = {key: placed.get(key) for key, value in task.submitted.items() if placed.get(key) != value}
        if wrong:
            return "the order has " + ", ".join(f"{key}={value!r}" for key, value in wrong.items())
    if task.answer and task.answer.lower() not in answer.lower():
        return f"the answer does not say {task.answer!r}"
    if status is not None and status != (task.status or "complete"):
        return f"the run ended {status}"
    return ""


def hyprctl(*arguments: str) -> str:
    return subprocess.run(["hyprctl", *arguments], capture_output=True, text=True, check=True).stdout


def until_idle(timeout: float = 300.0) -> None:
    """Wait until no other program is using deskpilot's plugin, so this run's input is not queued behind theirs."""
    deadline = time.monotonic() + timeout
    while True:
        status = json.loads(hyprctl("deskpilot", "status"))
        if not status.get("busy") and not status.get("queued"):
            return
        if time.monotonic() >= deadline:
            raise SystemExit("deskpilot's plugin stayed busy; try again later")
        time.sleep(2)


class FocusWatch:
    """Samples the focused window until stopped, and remembers whether it was ever one of class `app_id`."""

    def __init__(self, app_id: str) -> None:
        self.app_id = app_id
        self.taken = False
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._watch, daemon=True)

    def _watch(self) -> None:
        while not self._stop.wait(0.2):
            active = json.loads(hyprctl("activewindow", "-j") or "{}")
            if active.get("class") == self.app_id:
                self.taken = True

    def __enter__(self) -> FocusWatch:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._stop.set()
        self._thread.join()


class Form:
    """A fresh order form for an arm to launch, its state file cleared first and its process ended on the way out."""

    def __init__(self, out: Path, layout: str, python: str) -> None:
        self.state = out / "state.json"
        # The class the form's window takes, which no other window has.
        self.app_id = f"fastbrowse-evals-{uuid.uuid4().hex[:8]}"
        # The system's Python, which has PyGObject; the harness's own environment does not.
        self.argv = [python, str(FORM), str(self.state), self.app_id, "--layout", layout]
        self.command = shlex.join(self.argv)

    def __enter__(self) -> Form:
        self.state.unlink(missing_ok=True)
        return self

    def saved(self) -> dict[str, Any] | None:
        try:
            return json.loads(self.state.read_text())
        except (OSError, json.JSONDecodeError):
            return None

    def __exit__(self, *_: object) -> None:
        # The arm's deskpilot closes the form it launched when it exits; one with a popup open, or an arm killed at
        # its timeout before deskpilot could, leaves the process behind.
        subprocess.run(["pkill", "-f", f"order_form.py .* {self.app_id} "], check=False)


def _terminated(signum: int, frame: object) -> None:
    raise SystemExit(128 + signum)


def spawn(argv: list[str], out: Path, arm: str, timeout: float, env: dict[str, str] | None = None) -> float:
    """Runs one arm to the end or the timeout, its stdout and stderr kept in `out`; returns its wall seconds."""
    began = time.monotonic()
    with (out / f"{arm}.out").open("w") as stdout, (out / f"{arm}.err").open("w") as stderr:
        child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL, env=env)
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            pass
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=CLEANUP_GRACE)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
    return time.monotonic() - began


def fastbrowse_arm(binary: str, task: DesktopTask, form: Form, out: Path, timeout: float) -> dict[str, Any]:
    trace = out / "fastbrowse.jsonl"
    trace.unlink(missing_ok=True)
    argv = [binary, "--desktop", form.command, "--json", "--trace", str(trace)]
    if task.authorize:
        argv.append("--authorize")
    wall = spawn([*argv, task.task], out, "fastbrowse", timeout)
    try:
        result = json.loads((out / "fastbrowse.out").read_text() or "{}")
    except json.JSONDecodeError:
        result = {}
    calls = (
        sum(1 for line in trace.read_text().splitlines() if json.loads(line).get("event") == "call")
        if (trace.exists())
        else 0
    )
    lines = (result.get("cost") or {}).get("lines") or []
    return {
        "status": result.get("status", "no result"),
        "answer": result.get("answer") or "",
        "wall": wall,
        "dollars": sum(line.get("dollars") or 0.0 for line in lines),
        "calls": calls,
        "error": result.get("error") or ((out / "fastbrowse.err").read_text()[-500:] if not result else None),
    }


def claude_arm(model: str, task: DesktopTask, form: Form, out: Path, timeout: float) -> dict[str, Any]:
    config = out / "mcp.json"
    # deskpilot sends hidden input only to an app its named agent launched.
    server = {"command": "deskpilot", "args": ["mcp"], "env": {"DESKPILOT_AGENT": form.app_id}}
    config.write_text(json.dumps({"mcpServers": {"deskpilot": server}}))
    command = json.dumps(form.argv)
    prompt = CLAUDE_PROMPT.format(command=command, gate=GATE[0 if task.authorize else 1], task=task.task)
    argv = [
        "claude",
        "-p",
        prompt,
        "--model",
        model,
        "--output-format",
        "json",
        "--mcp-config",
        str(config),
        "--strict-mcp-config",
        "--tools",
        "",
        "--allowedTools",
        "mcp__deskpilot",
        "--no-session-persistence",
    ]
    wall = spawn(argv, out, "claude", timeout)
    try:
        result = json.loads((out / "claude.out").read_text() or "{}")
    except json.JSONDecodeError:
        result = {}
    return {
        # Claude reports no status of its own: an answer is `complete`, anything else what its CLI said.
        "status": None,
        "answer": result.get("result") or "",
        "wall": wall,
        "dollars": float(result.get("total_cost_usd") or 0.0),
        "calls": int(result.get("num_turns") or 0),
        "error": None if result.get("result") else ((out / "claude.err").read_text()[-500:] or "no result"),
    }


def run(task: DesktopTask, arm: str, out: Path, args: argparse.Namespace) -> Measured:
    until_idle()
    with Form(out, task.layout, args.python) as form, FocusWatch(form.app_id) as watch:
        if arm == "fastbrowse":
            ran = fastbrowse_arm(args.fastbrowse, task, form, out, args.timeout)
        else:
            ran = claude_arm(args.claude_model, task, form, out, args.timeout)
        state = form.saved()
    (out / f"{arm}.state.json").write_text(json.dumps(state, ensure_ascii=False, indent=2) + "\n")
    why = grade(task, state, ran["answer"], ran["status"])
    if watch.taken:
        why = (why + "; " if why else "") + "the window took keyboard focus"
    return Measured(
        task=task.id,
        arm=arm,
        passed=not why,
        status=ran["status"] or ("answered" if ran["answer"] else "no answer"),
        answer=ran["answer"],
        wall=ran["wall"],
        dollars=ran["dollars"],
        calls=ran["calls"],
        focused=watch.taken,
        grade=why,
        known_limit=task.known_limit,
        error=ran["error"],
    )


def table(rows: list[Measured]) -> str:
    lines = [
        "| Task | Arm | Passed | Status | Wall s | Cost $ | Model calls (Claude: turns) | Why it failed |",
        "| --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        why = row.grade + (f" (known: {row.known_limit})" if row.grade and row.known_limit else "")
        lines.append(
            f"| {row.task} | {row.arm} | {'yes' if row.passed else 'no'} | {row.status} | {row.wall:.0f} | "
            f"{row.dollars:.4f} | {row.calls} | {why.replace('|', '/')} |"
        )
    for arm in sorted({row.arm for row in rows}):
        mine = [row for row in rows if row.arm == arm]
        lines.append(
            f"\n{arm}: {sum(row.passed for row in mine)} of {len(mine)} passed, "
            f"${sum(row.dollars for row in mine):.4f}, {sum(row.wall for row in mine):.0f} s"
        )
    return "\n".join(lines)


def main(arguments: list[str]) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals desktop", description=__doc__)
    parser.add_argument("out", type=Path, help="directory for each run's result, trace and the form's state")
    parser.add_argument("--fastbrowse", default=os.environ.get("FASTBROWSE_BIN", "fastbrowse"))
    parser.add_argument("--arm", action="append", choices=ARMS, help="run only these arms (repeatable)")
    parser.add_argument("--task", action="append", help="run only these task ids (repeatable)")
    parser.add_argument("--claude-model", default="sonnet")
    parser.add_argument("--python", default="/usr/bin/python3", help="a Python with PyGObject and GTK 3, for the form")
    parser.add_argument("--timeout", type=float, default=300.0)
    args = parser.parse_args(arguments)
    if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
        raise SystemExit("the desktop benchmark needs a Hyprland session with deskpilot's plugin loaded")
    rows: list[Measured] = []
    # SIGTERM would end the benchmark without closing its form or stopping its arm: raise instead, as SIGINT does.
    previous = signal.signal(signal.SIGTERM, _terminated)
    try:
        for task in TASKS:
            if args.task and task.id not in args.task:
                continue
            for arm in args.arm or ARMS:
                out = args.out / task.id / arm
                out.mkdir(parents=True, exist_ok=True)
                row = run(task, arm, out, args)
                rows.append(row)
                sys.stdout.write(f"{row.task} {row.arm}: {'pass' if row.passed else 'FAIL ' + row.grade}\n")
                sys.stdout.flush()
    finally:
        signal.signal(signal.SIGTERM, previous)
        (args.out / "summary.json").write_text(json.dumps([asdict(row) for row in rows], indent=2) + "\n")
        (args.out / "summary.md").write_text(table(rows) + "\n")
    sys.stdout.write(table(rows) + "\n")
