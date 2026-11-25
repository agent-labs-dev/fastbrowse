"""Fastbrowse's benchmarks: `fastbrowse-evals browser <command>`, each command with its own `--help`."""

from __future__ import annotations

import argparse
import importlib
import sys
from pathlib import Path

from fastbrowse_evals.catalog import export
from fastbrowse_evals.versions import relock

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
    actions.add_parser("relock", help="recompute every task's lock fingerprint from its Parallax record")
    for name in FORWARDED:
        actions.add_parser(name, add_help=False, help=f"{name} benchmark command; use --help for its options")
    return root


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    # A forwarded command's flags, its --help included, belong to its own parser.
    if len(arguments) >= 2 and arguments[0] == "browser" and arguments[1] in FORWARDED:
        return run(arguments[1], arguments[2:])
    args = parser().parse_args(arguments)
    if args.action == "relock":
        relock()
        return 0
    export(args.path, check=args.check)
    return 0
