"""Fastbrowse's benchmarks: `fastbrowse-evals browser <command>`, each command with its own `--help`, and
`fastbrowse-evals desktop` for desktop apps."""

from __future__ import annotations

import argparse
import asyncio
import importlib
import signal
import sys
from collections.abc import Coroutine
from pathlib import Path
from typing import Any

from fastbrowse_evals.catalog import export

FORWARDED = {
    "internal": "live",
    "official": "official",
    "publish-candidate": "publication",
    "track-ledger": "track_ledger",
    "track-official": "official_tracking",
    "official-rows": "official_rows",
    "recipes": "recipes",
}
"""Commands whose module parses its own arguments, by the module that runs them."""
ASYNC = frozenset({"internal", "official"})

TERMINATED = 128 + signal.SIGTERM
"""The exit code of a benchmark command stopped by SIGTERM, as a shell reports a process the signal ended."""


async def until_terminated(work: Coroutine[Any, Any, int]) -> int:
    """Await `work`, cancelling it on SIGTERM instead of dying where it stands.

    Python's default SIGTERM ends the process at once: no `finally`, no `async with` exit. Every cloud browser in
    flight then bills until its own 240-minute timeout, and an adapter runner, which runs in a session of its own,
    outlives the harness and keeps driving its browser. Cancelling takes the path Ctrl-C does, which stops each
    browser, kills each runner and writes each interrupted attempt to the ledger."""
    task = asyncio.ensure_future(work)
    loop = asyncio.get_running_loop()
    terminated = False

    def terminate() -> None:
        nonlocal terminated
        terminated = True
        task.cancel()

    loop.add_signal_handler(signal.SIGTERM, terminate)
    try:
        return await task
    except asyncio.CancelledError:
        if not terminated:
            raise
        return TERMINATED
    finally:
        loop.remove_signal_handler(signal.SIGTERM)


def run(action: str, arguments: list[str]) -> int:
    module = importlib.import_module(f"fastbrowse_evals.{FORWARDED[action]}")
    if action in ASYNC:
        return asyncio.run(until_terminated(module.main(arguments)))
    module.main(arguments)
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(prog="fastbrowse-evals", description=__doc__)
    families = root.add_subparsers(dest="family", required=True)
    browser = families.add_parser("browser", help="the internal campaign, its catalog and candidate, official suites")
    actions = browser.add_subparsers(dest="action", required=True)
    catalog = actions.add_parser("catalog", help="export or verify the public task metadata")
    catalog.add_argument("path", type=Path)
    catalog.add_argument("--check", action="store_true")
    for name in FORWARDED:
        actions.add_parser(name, add_help=False, help=f"{name} benchmark command; use --help for its options")
    families.add_parser("desktop", add_help=False, help="desktop app tasks through deskpilot; use --help for options")
    return root


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    # A forwarded command's flags, its --help included, belong to its own parser.
    if len(arguments) >= 2 and arguments[0] == "browser" and arguments[1] in FORWARDED:
        return run(arguments[1], arguments[2:])
    if arguments[:1] == ["desktop"]:
        importlib.import_module("fastbrowse_evals.desktop.bench").main(arguments[1:])
        return 0
    args = parser().parse_args(arguments)
    export(args.path, check=args.check)
    return 0
