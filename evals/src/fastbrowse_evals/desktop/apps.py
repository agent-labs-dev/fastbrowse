"""`fastbrowse --desktop` on real desktop apps, each task checked on what it left behind: `fastbrowse-evals desktop-apps`.

`bench.py` measures one order form built for the purpose. This runs the apps people use (GNOME Text Editor, Files and
Calculator, CMake's Qt 6 GUI, the Qt 5 settings tool, Chrome, a Zenity dialog, a terminal) across the kinds of work a
desktop asks for: typing and saving, a file dialog, find and replace, tabs, menus, a context menu, a list, a drag
and drop, a keyboard shortcut, a toggle, a modal dialog, a value carried between two apps, a value read, and a task
the app cannot do.

A task is graded on the files the app wrote, the request a page sent, or the dialog's exit code, read from the run's
own directory with no model. Only a read-only task's answer is graded, by the LLM judge. Every run ends as one of:

- `pass`: done, and reported complete (an impossible task: nothing changed and not reported complete)
- `unreported`: done, but reported as not complete
- `fail`: not done, and not reported complete
- `false complete`: reported complete with the work not done. One of these fails the whole eval.

Each run has its own home, data and config directories, portals off and settings kept in memory, so an app reads
and writes nothing of the user's. Apps are started hidden by deskpilot; run it inside a throwaway compositor
(deskpilot's `scripts/nested.py start`, then `HYPRLAND_INSTANCE_SIGNATURE` and `WAYLAND_DISPLAY` from what it prints)
and nothing reaches the user's screen. A task whose app is not installed is skipped and named.
"""

from __future__ import annotations

import argparse
import asyncio
import functools
import http.server
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import httpx
from fastbrowse.evals.judge import Claim, jev_from_environment
from fastbrowse.evals.judge import judge as judge_claims

from fastbrowse_evals.desktop.bench import spawn

OUTCOMES = ("pass", "unreported", "fail", "false complete")
NO_RESULT = "no result"
EDITOR = ["gnome-text-editor", "-s"]
QT = ["env", "QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1", "QT_QPA_PLATFORM=wayland"]
CHROME = [
    "google-chrome-stable",
    "--no-first-run",
    "--no-default-browser-check",
    "--password-store=basic",
    "--disable-sync",
    "--force-renderer-accessibility",
    "--ozone-platform=wayland",
]
# The bus name a single-instance app owns: a second copy hands its files to the first and exits, so a run waits for
# the name to be free.
BUS_NAMES = {"gnome-calculator": "org.gnome.Calculator", "nautilus": "org.gnome.Nautilus"}

Setup = Callable[[Path, int], list[list[str]]]
"""Writes a task's fixtures into the run's work directory and returns the command line of each app to start. The
integer is the port of the run's page server, which serves `site/` and appends each POST body to `posts.jsonl`."""
Fault = Callable[[Path], str]
"""Why the work directory does not show the task done, or "" when it does."""


@dataclass(frozen=True)
class AppTask:
    id: str
    kind: str
    task: str
    setup: Setup
    fault: Fault
    authorize: bool = False
    claim: str | None = None
    """A sentence the judge decides of a read-only task's answer."""
    impossible: bool = False
    """The app cannot do it: the run passes when nothing changed and it did not report complete."""


def write(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def read(path: Path) -> str | None:
    try:
        return path.read_text()
    except OSError:
        return None


def holds(path: Path, want: str) -> str:
    """The file's text against `want`, each without its trailing blank lines: an editor may add or drop the last
    newline."""
    got = read(path)
    if got is not None and got.rstrip("\n") == want.rstrip("\n"):
        return ""
    return f"{path.name} holds {got!r}"


def listing(directory: Path) -> list[str]:
    return sorted(entry.name + ("/" if entry.is_dir() else "") for entry in directory.iterdir())


def posted(work: Path) -> Any:
    """The last request body a page of the run sent, or None."""
    lines = (read(work / "posts.jsonl") or "").splitlines()
    return json.loads(lines[-1]) if lines else None


def editor(name: str, text: str, *more: tuple[str, str]) -> Setup:
    def setup(work: Path, port: int) -> list[list[str]]:
        return [EDITOR + [str(write(work / file, body)) for file, body in ((name, text), *more)]]

    return setup


NOTES = "Call the plumber on Monday\nRenew the car insurance\nBook dentist for Sam\nReturn library books\n"
LETTER = (
    "Dear Mr Smith,\n\nThank you, Mr Smith, for your order.\nWe will write to Mr Smith again when it ships.\n\n"
    "Regards to Mr Smith and family.\n"
)
ORDERS = [f"2026-11-18 {8 + i // 60:02d}:{i % 60:02d}:12 order {49122 + i} SHIPPED carrier ok" for i in range(240)]
ORDERS[187] = "2026-11-18 11:06:12 order 49309 FAILED payment declined code E7719 retry scheduled"
ORDER_LOG = "\n".join(ORDERS) + "\n"


def saved_as(work: Path) -> str:
    return holds(work / "notes-backup.txt", NOTES) or holds(work / "notes.txt", NOTES)


def second_tab(work: Path) -> str:
    return holds(work / "second.txt", "one\ntwo\ndone\n") or holds(work / "first.txt", "alpha\nbeta\n")


def scans(*extra: str) -> Setup:
    def setup(work: Path, port: int) -> list[list[str]]:
        for name in ("IMG_4471.txt", "IMG_4472.txt", "notes.txt", *extra):
            if name.endswith("/"):
                (work / "scans" / name).mkdir(parents=True, exist_ok=True)
            else:
                write(work / "scans" / name, f"contents of {name}\n")
        return [["nautilus", "--new-window", str(work / "scans")]]

    return setup


def lists(want: list[str], directory: str = "scans") -> Fault:
    def fault(work: Path) -> str:
        got = listing(work / directory)
        return "" if got == want else f"{directory} holds {got}"

    return fault


def calculator_and_editor(work: Path, port: int) -> list[list[str]]:
    return [["gnome-calculator"], [*EDITOR, str(write(work / "result.txt", ""))]]


def result_is_391(work: Path) -> str:
    return holds(work / "result.txt", "391")


CMAKELISTS = (
    "cmake_minimum_required(VERSION 3.20)\nproject(demo NONE)\n"
    'set(GREETING "bonjour-42" CACHE STRING "A greeting")\noption(ENABLE_LOGS "Enable logs" OFF)\n'
)


def cmake(work: Path, port: int) -> list[list[str]]:
    if shutil.which("cmake") is None:
        return [["cmake"]]
    write(work / "src/CMakeLists.txt", CMAKELISTS)
    subprocess.run(["cmake", "-S", str(work / "src"), "-B", str(work / "build")], capture_output=True, check=True)
    return [[*QT, "cmake-gui", "-S", str(work / "src"), "-B", str(work / "build")]]


def cache_line(line: str) -> Fault:
    def fault(work: Path) -> str:
        key = line.split(":", maxsplit=1)[0]
        cache = (read(work / "build/CMakeCache.txt") or "").splitlines()
        return "" if line in cache else f"the cache has {[row for row in cache if row.split(':')[0] == key]}"

    return fault


def no_cache(work: Path) -> str:
    return "CMakeCache.txt is still there" if (work / "build/CMakeCache.txt").exists() else ""


def qt5ct(work: Path, port: int) -> list[list[str]]:
    return [[*QT, "qt5ct"]]


def double_click_650(work: Path) -> str:
    # The run's XDG_CONFIG_HOME is beside its work directory.
    conf = (read(work.parent / "config/qt5ct/qt5ct.conf") or "").splitlines()
    return "" if "double_click_interval=650" in conf else f"qt5ct.conf has {[r for r in conf if 'double_click' in r]}"


PAGE = (
    "<!doctype html><meta charset=utf-8><title>{title}</title><style>body{{font:16px sans-serif;margin:32px}}"
    "button,input{{font:inherit;padding:6px}}label{{display:block;margin:10px 0}}</style><h1>{title}</h1>{body}"
    "<p id=status></p><script>function save(o){{return fetch('/save',{{method:'POST',body:JSON.stringify(o)}})"
    ".then(()=>{{document.getElementById('status').textContent='Saved'}})}}</script>"
)


def two_tabs(work: Path, port: int) -> list[list[str]]:
    write(work / "site/welcome.html", PAGE.format(title="Welcome", body="<p>Nothing to do on this page.</p>"))
    form = (
        "<label>Item <input id=item value='Desk lamp' readonly></label>"
        "<label>Quantity <input id=qty type=number value=1 aria-label=Quantity></label>"
        "<button onclick=\"save({page:'order',qty:qty.value})\">Submit order</button>"
    )
    write(work / "site/order.html", PAGE.format(title="Order form", body=form))
    pages = [f"http://127.0.0.1:{port}/{name}.html" for name in ("welcome", "order")]
    return [[*CHROME, f"--user-data-dir={work.parent}/chrome", *pages]]


def ordered_three(work: Path) -> str:
    sent = posted(work)
    return "" if sent == {"page": "order", "qty": "3"} else f"the page sent {sent}"


def question(work: Path, port: int) -> list[list[str]]:
    script = 'zenity --question --title="Clean up" --text="Delete all temporary files now?"; echo $? > "$1"'
    return [["sh", "-c", script, "sh", str(work / "answer.txt")]]


def answered_no(work: Path) -> str:
    return holds(work / "answer.txt", "1")


def terminal(work: Path, port: int) -> list[list[str]]:
    for name, size in (("db-full.tar", 48213), ("photos.tar", 9120), ("notes.tar", 733)):
        (work / "backups").mkdir(exist_ok=True)
        (work / "backups" / name).write_bytes(b"x" * size)
    return [["kitty", "--config", "NONE", "-o", "confirm_os_window_close=0", "--directory", str(work)]]


def nothing(work: Path) -> str:
    return ""


TASKS = (
    AppTask(
        id="editor-type-save",
        kind="type and save",
        task="Add milk as a new last line of shopping.txt and save the file.",
        setup=editor("shopping.txt", "bread\neggs\ncoffee\n"),
        fault=lambda work: holds(work / "shopping.txt", "bread\neggs\ncoffee\nmilk\n"),
    ),
    AppTask(
        id="editor-save-as",
        kind="file dialog",
        task="Save a copy of notes.txt under the name notes-backup.txt in the same folder.",
        setup=editor("notes.txt", NOTES),
        fault=saved_as,
    ),
    AppTask(
        id="editor-replace",
        kind="find and replace",
        task="In letter.txt replace every Mr Smith with Dr Jones and save the file.",
        setup=editor("letter.txt", LETTER),
        fault=lambda work: holds(work / "letter.txt", LETTER.replace("Mr Smith", "Dr Jones")),
    ),
    AppTask(
        id="editor-tabs",
        kind="tabs",
        task=(
            "first.txt and second.txt are open as tabs: add the word done as a new last line of second.txt and save it."
        ),
        setup=editor("first.txt", "alpha\nbeta\n", ("second.txt", "one\ntwo\n")),
        fault=second_tab,
    ),
    AppTask(
        id="editor-context-menu",
        kind="context menu",
        task="Change all the text in shout.txt to upper case using the editor's Change Case menu and save it.",
        setup=editor("shout.txt", "please keep the noise down\nthank you\n"),
        fault=lambda work: holds(work / "shout.txt", "PLEASE KEEP THE NOISE DOWN\nTHANK YOU\n"),
    ),
    AppTask(
        id="editor-read-scrolled",
        kind="read a value out of view",
        task="Find order 49309 in orders.log and tell me what happened to it and its error code.",
        setup=editor("orders.log", ORDER_LOG),
        fault=lambda work: holds(work / "orders.log", ORDER_LOG),
        claim="The answer says order 49309 failed because the payment was declined, with error code E7719.",
    ),
    AppTask(
        id="editor-impossible",
        kind="impossible task",
        task="Use the text editor's Insert Table command to add a table with 3 columns to notes.txt and save it.",
        setup=editor("notes.txt", NOTES),
        fault=lambda work: holds(work / "notes.txt", NOTES),
        impossible=True,
    ),
    AppTask(
        id="files-rename",
        kind="list",
        task="In the scans folder rename IMG_4471.txt to rent-receipt-march.txt.",
        setup=scans(),
        fault=lists(["IMG_4472.txt", "notes.txt", "rent-receipt-march.txt"]),
    ),
    AppTask(
        id="files-drag",
        kind="drag and drop",
        task="Drag photo-1.txt onto the keep folder in the scans folder so it moves there.",
        setup=scans("photo-1.txt", "keep/"),
        fault=lists(["photo-1.txt"], "scans/keep"),
    ),
    AppTask(
        id="files-shortcut",
        kind="keyboard shortcut",
        task="Press Ctrl+Shift+N in the scans folder to make a new folder and call it inbox.",
        setup=scans(),
        fault=lists(["IMG_4471.txt", "IMG_4472.txt", "inbox/", "notes.txt"]),
    ),
    AppTask(
        id="calculator-to-editor",
        kind="carry a value between apps",
        task="Work out 17 times 23 in the calculator, write the result in result.txt and save it.",
        setup=calculator_and_editor,
        fault=result_is_391,
    ),
    AppTask(
        id="cmake-toggle",
        kind="toggle",
        task="Turn on the ENABLE_LOGS option of this CMake project and press Configure.",
        setup=cmake,
        fault=cache_line("ENABLE_LOGS:BOOL=ON"),
    ),
    AppTask(
        id="cmake-add-entry",
        kind="modal dialog",
        task=(
            "Add a new cache entry named BUILD_LABEL of type STRING with the value nightly to this CMake project, "
            "then press Configure."
        ),
        setup=cmake,
        fault=cache_line("BUILD_LABEL:STRING=nightly"),
    ),
    AppTask(
        id="cmake-menu-delete-cache",
        kind="menu",
        task="Use the File menu to delete this CMake project's cache.",
        setup=cmake,
        fault=no_cache,
        authorize=True,
    ),
    AppTask(
        id="qt5ct-tabs",
        kind="tabs",
        task="In the Qt5 settings tool open the Interface tab, set the double click interval to 650 ms and press Apply.",
        setup=qt5ct,
        fault=double_click_650,
    ),
    AppTask(
        id="chrome-tabs",
        kind="tabs",
        task="The order form is open in the second browser tab: set the quantity to 3 and submit the order.",
        setup=two_tabs,
        fault=ordered_three,
        authorize=True,
    ),
    AppTask(
        id="zenity-question",
        kind="modal dialog",
        task="A dialog is asking whether to delete all temporary files: answer No.",
        setup=question,
        fault=answered_no,
    ),
    AppTask(
        id="terminal-read",
        kind="app with no accessibility tree",
        task="In the terminal, list the backups folder with file sizes and tell me which file is the largest.",
        setup=terminal,
        fault=nothing,
        claim="The answer says the largest file is db-full.tar.",
    ),
)


def outcome(task: AppTask, status: str, fault: str) -> str:
    """How a run ended, from the status fastbrowse reported and why its work is not done ("" when it is)."""
    complete = status == "complete"
    if task.impossible:
        # A run that reported nothing, a crash or a timeout, declined nothing.
        return "false complete" if complete else "fail" if fault or status == NO_RESULT else "pass"
    if complete:
        return "false complete" if fault else "pass"
    return "fail" if fault else "unreported"


class Pages(http.server.SimpleHTTPRequestHandler):
    """Serves the run's `site/` and appends each POST body to `posts.jsonl`, for a page task to be graded on."""

    posts: Path

    def do_POST(self) -> None:
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with self.posts.open("a") as out:
            out.write(body.decode() + "\n")
        self.send_response(200)
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, format: str, *args: Any) -> None:
        pass


def serve(work: Path) -> http.server.ThreadingHTTPServer:
    (work / "site").mkdir(parents=True, exist_ok=True)
    handler = type("RunPages", (Pages,), {"posts": work / "posts.jsonl"})
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(handler, directory=str(work / "site")))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def started_by(run: Path) -> list[int]:
    """The processes whose environment names this run's directory: the apps this run started and no others."""
    needle = str(run).encode()
    found = []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and int(entry.name) != os.getpid():
            try:
                if needle in (entry / "environ").read_bytes():
                    found.append(int(entry.name))
            except OSError:
                continue
    return found


def stop_apps(run: Path) -> None:
    """Ends what the run left behind: an app with a dialog open outlives the deskpilot that started it."""
    for sent in (signal.SIGTERM, signal.SIGKILL):
        pids = started_by(run)
        if not pids:
            return
        for pid in pids:
            try:
                os.kill(pid, sent)
            except OSError:
                continue
        time.sleep(1.5)


def bus_owned(name: str) -> bool:
    return subprocess.run(["busctl", "--user", "status", name], capture_output=True, check=False).returncode == 0


def missing(commands: list[list[str]]) -> list[str]:
    """The programs of `commands` that are not installed, by the first word that is not `env` or its settings."""
    absent = []
    for command in commands:
        words = [word for word in command if word != "env" and "=" not in word]
        if words and shutil.which(words[0]) is None:
            absent.append(words[0])
    return absent


@dataclass
class Ran:
    task: str
    kind: str
    repeat: int
    outcome: str
    status: str
    why: str
    wall: float
    dollars: float
    answer: str


async def answer_fault(task: AppTask, answer: str) -> str:
    if task.claim is None:
        return ""
    async with httpx.AsyncClient(timeout=60.0) as http:
        jev = jev_from_environment(http)
        if await judge_claims(jev, task=task.task, answer=answer, claims=(Claim(task.claim),)) is None:
            return ""
    return f"the answer does not hold: {task.claim}"


def run(task: AppTask, repeat: int, out: Path, args: argparse.Namespace) -> Ran | None:
    """One run of one task in its own directory, or None when an app it needs is not installed."""
    if out.exists() and any(out.iterdir()):
        # What an earlier run left, a moved file or a page's request, would be graded as this run's work.
        raise SystemExit(f"{out} holds an earlier run: give a new directory")
    work = out / "work"
    for name in ("work", "home", "data", "config"):
        (out / name).mkdir(parents=True, exist_ok=True)
    server = serve(work)
    try:
        commands = task.setup(work, server.server_address[1])
        if absent := missing(commands):
            sys.stdout.write(f"{task.id}: skipped, not installed: {', '.join(absent)}\n")
            return None
        for command in commands:
            name = BUS_NAMES.get(command[0])
            deadline = time.monotonic() + 30
            while name and bus_owned(name) and time.monotonic() < deadline:
                time.sleep(1)
            if name and bus_owned(name):
                raise SystemExit(f"{name} is already on the session bus: close that app and run again")
        private = [
            "env",
            f"HOME={out}/home",
            f"XDG_DATA_HOME={out}/data",
            f"XDG_CONFIG_HOME={out}/config",
            "GTK_USE_PORTAL=0",
            "GDK_DEBUG=no-portals",
            "GSETTINGS_BACKEND=memory",
        ]
        argv = [args.fastbrowse, task.task, "--json", "--trace", str(out / "trace.jsonl")]
        for command in commands:
            argv += ["--desktop", shlex.join(private + command)]
        if task.authorize:
            argv.append("--authorize")
        if args.record:
            argv += ["--record", str(out / "clip.mp4")]
        wall = spawn(argv, out, "fastbrowse", args.timeout)
        # A dialog's script writes its exit code after the window closes.
        time.sleep(1.0)
    finally:
        stop_apps(out)
        server.shutdown()
    try:
        result = json.loads((out / "fastbrowse.out").read_text() or "{}")
    except json.JSONDecodeError:
        result = {}
    status = result.get("status") or NO_RESULT
    answer = result.get("answer") or ""
    why = task.fault(work)
    if not why and status == "complete":
        why = asyncio.run(answer_fault(task, answer))
    lines = (result.get("cost") or {}).get("lines") or []
    ran = Ran(
        task=task.id,
        kind=task.kind,
        repeat=repeat,
        outcome=outcome(task, status, why),
        status=status,
        why=why or (result.get("error") or ""),
        wall=wall,
        dollars=sum(line.get("dollars") or 0.0 for line in lines),
        answer=answer,
    )
    (out / "result.json").write_text(json.dumps(asdict(ran), ensure_ascii=False, indent=2) + "\n")
    return ran


def table(rows: list[Ran]) -> str:
    lines = [
        "| Task | Kind | Pass | Unreported | Fail | False complete | Cost $ |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for task in dict.fromkeys(row.task for row in rows):
        mine = [row for row in rows if row.task == task]
        counts = " | ".join(str(sum(row.outcome == name for row in mine)) for name in OUTCOMES)
        lines.append(f"| {task} | {mine[0].kind} | {counts} | {sum(row.dollars for row in mine):.4f} |")
    totals = ", ".join(f"{sum(row.outcome == name for row in rows)} {name}" for name in OUTCOMES)
    lines.append(f"\n{len(rows)} runs: {totals}, ${sum(row.dollars for row in rows):.4f}")
    return "\n".join(lines)


def main(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals desktop-apps", description=__doc__)
    parser.add_argument("out", type=Path, help="directory for each run's work files, result and trace")
    parser.add_argument("--fastbrowse", default=os.environ.get("FASTBROWSE_BIN", "fastbrowse"))
    parser.add_argument("--task", action="append", help="run only these task ids (repeatable)")
    parser.add_argument("--repeat", type=int, default=1, help="runs of each task")
    parser.add_argument("--timeout", type=float, default=180.0, help="seconds one run may take")
    parser.add_argument("--record", action="store_true", help="film each run to clip.mp4 in its directory")
    args = parser.parse_args(arguments)
    if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
        raise SystemExit("the desktop apps eval needs a Hyprland session with deskpilot's plugin loaded")
    rows: list[Ran] = []
    try:
        for task in TASKS:
            if args.task and task.id not in args.task:
                continue
            for repeat in range(1, args.repeat + 1):
                ran = run(task, repeat, (args.out / task.id / str(repeat)).resolve(), args)
                if ran is None:
                    break
                rows.append(ran)
                sys.stdout.write(f"{ran.task} {repeat}: {ran.outcome}{' (' + ran.why + ')' if ran.why else ''}\n")
                sys.stdout.flush()
    finally:
        (args.out / "summary.json").write_text(json.dumps([asdict(row) for row in rows], indent=2) + "\n")
        (args.out / "summary.md").write_text(table(rows) + "\n")
    sys.stdout.write(table(rows) + "\n")
    return 1 if any(row.outcome == "false complete" for row in rows) else 0
