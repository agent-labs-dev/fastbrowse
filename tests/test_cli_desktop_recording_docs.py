"""`--desktop` accepts `--record` (#1335), so neither the help nor the reference may list it as refused."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def _desktop_refusals(text: str, start: str, end: str) -> set[str]:
    clause = text.split(start, 1)[1].split(end, 1)[0]
    return set(re.findall(r"--[a-z-]+", clause))


def test_help_does_not_list_record_as_a_desktop_refusal() -> None:
    help_text = (ROOT / "crates/fastbrowse-cli/generated/cli.txt").read_text(encoding="utf-8")
    refused = _desktop_refusals(help_text, "--desktop drives", "\n\n")
    assert "--start" in refused
    assert "--record" not in refused


def test_reference_does_not_list_record_as_a_desktop_refusal() -> None:
    reference = (ROOT / "docs/cli.md").read_text(encoding="utf-8")
    refused = _desktop_refusals(reference, "`--desktop` drives an app", ";")
    assert "--start" in refused
    assert "--record" not in refused
