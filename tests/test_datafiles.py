import subprocess
import sys
from pathlib import Path

import pytest

from fastbrowse.datafiles import data_file


@pytest.mark.parametrize(
    ("parts", "opening"),
    [
        (("browser", "snapshot.js"), "// Ported from jev-ultrafast (MIT)"),
        (("browser", "capture.js"), "// Complete structured text capture"),
        (("browser", "autoconsent", "autoconsent.standalone.js"), '"use strict";'),
    ],
)
def test_a_checkout_serves_the_page_scripts_and_the_autoconsent_bundle(parts: tuple[str, ...], opening: str) -> None:
    assert data_file(*parts).read_text(encoding="utf-8").lstrip().startswith(opening)


def test_a_frozen_interpreter_serves_them_from_its_bundle(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    bundled = tmp_path / "fastbrowse" / "browser" / "snapshot.js"
    bundled.parent.mkdir(parents=True)
    bundled.write_text("bundled snapshot", encoding="utf-8")
    monkeypatch.setattr(sys, "frozen", True, raising=False)
    monkeypatch.setattr(sys, "_MEIPASS", str(tmp_path), raising=False)

    assert data_file("browser", "snapshot.js").read_text(encoding="utf-8") == "bundled snapshot"


def test_every_reader_loads_its_scripts_from_the_bundle_when_frozen(tmp_path: Path) -> None:
    browser = tmp_path / "fastbrowse" / "browser"
    (browser / "autoconsent").mkdir(parents=True)
    (browser / "snapshot.js").write_text("bundled-snapshot", encoding="utf-8")
    (browser / "capture.js").write_text("bundled-capture", encoding="utf-8")
    (browser / "autoconsent" / "autoconsent.standalone.js").write_text("bundled-autoconsent", encoding="utf-8")
    script = (
        "import sys\n"
        f"sys.frozen, sys._MEIPASS = True, {str(tmp_path)!r}\n"
        "from fastbrowse.browser import page, session\n"
        "from fastbrowse.evals import observe\n"
        "print(page._PAGE_JS, page._CAPTURE_JS, session._TRACK_DOCUMENT_JS, session._REFUSE_COOKIES_JS,\n"
        "      observe.FINAL_SCRIPTS['snapshot'], sep='\\n')\n"
    )

    loaded = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, check=True).stdout

    assert loaded.splitlines() == [
        "bundled-snapshot",
        "bundled-capture",
        "bundled-snapshot('fingerprint')",
        "bundled-autoconsent",
        "bundled-snapshot('snapshot')",
    ]
