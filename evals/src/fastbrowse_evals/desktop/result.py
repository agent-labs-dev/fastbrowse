"""Reading one `fastbrowse --desktop --json` run: shared by the on-host benchmark (`bench.py`) and the Parallax arm
(`harness.py`), so both report a run's status, answer, cost and calls the same way."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def read_result(text: str) -> dict[str, Any]:
    """The JSON a run printed, or an empty result when it printed none (a run killed at its timeout)."""
    try:
        result = json.loads(text or "{}")
    except json.JSONDecodeError:
        return {}
    return result if isinstance(result, dict) else {}


def cost_lines(result: dict[str, Any]) -> list[dict[str, Any]]:
    return list((result.get("cost") or {}).get("lines") or [])


def dollars(result: dict[str, Any]) -> float:
    return sum(line.get("dollars") or 0.0 for line in cost_lines(result))


def model_calls(trace: Path) -> int:
    """The model calls a run's `--trace` file records."""
    if not trace.exists():
        return 0
    return sum(1 for line in trace.read_text().splitlines() if json.loads(line).get("event") == "call")
