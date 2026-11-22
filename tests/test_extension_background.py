"""The extension's service worker, run in Node against a fake chrome API."""

import json
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).parents[1]


def test_a_tab_chrome_refuses_to_attach_is_released_at_once() -> None:
    # #1314: a refused attach left the tab the run opened open, and owned by the run, until the run ended.
    node = shutil.which("node")
    assert node, "Node runs the extension's service worker"
    done = subprocess.run(
        [node, ROOT / "tests" / "extension_background.mjs"],
        env={"BACKGROUND_JS": str(ROOT / "crates" / "fastbrowse-browser" / "extension" / "background.js")},
        capture_output=True,
        text=True,
        check=True,
    )
    seen = json.loads(done.stdout)
    assert seen["opened"] == seen["adopted"] == "Cannot attach to this target"
    # The tab the run opened is closed; the user's tab is ungrouped and left open.
    assert ["remove", 5] in seen["calls"]
    assert ["ungroup", 7] in seen["calls"]
    assert ["remove", 7] not in seen["calls"]
    assert seen["listed"] == []
    assert seen["owned"] == ["tab 5 does not belong to this run", "tab 7 does not belong to this run"]
