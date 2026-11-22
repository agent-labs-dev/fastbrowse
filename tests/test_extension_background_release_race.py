"""A run stopped during failed adoption must preserve the user's tab."""

import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).parents[1]


def test_failed_adoption_preserves_user_tab_when_run_stops_during_cleanup() -> None:
    node = shutil.which("node")
    assert node, "Node runs the extension's service worker"
    subprocess.run(
        [node, ROOT / "tests" / "extension_background_release_race.mjs"],
        capture_output=True,
        text=True,
        check=True,
    )
