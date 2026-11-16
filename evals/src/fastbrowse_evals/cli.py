"""Fastbrowse's benchmarks: `fastbrowse-evals browser <command>`, each command with its own `--help`, and
`fastbrowse-evals desktop` for desktop apps."""

from __future__ import annotations

import argparse
import importlib
import sys
from pathlib import Path

from fastbrowse_evals.catalog import export

FORWARDED = {
    "publish-candidate": "publication",
    "recipes": "recipes",
}
"""Commands whose module parses its own arguments, by the module that runs them."""


def run(action: str, arguments: list[str]) -> int:
    module = importlib.import_module(f"fastbrowse_evals.{FORWARDED[action]}")
    module.main(arguments)
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(prog="fastbrowse-evals", description=__doc__)
    families = root.add_subparsers(dest="family", required=True)
    browser = families.add_parser("browser", help="the Parallax campaign catalog, publication, and official suites")
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
