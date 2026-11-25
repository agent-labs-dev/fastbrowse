"""A select's options must not replace the section that names its record."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


@pytest.mark.parametrize("card", ["li", "div class=field-card"])
def test_select_options_do_not_name_the_surrounding_record(card: str) -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    tag = card.split()[0]
    html = (
        "<section><h2>Settings</h2><ul>"
        f'<{card}><select aria-label="Primary country"><option>Japan</option><option>France</option></select></{tag}>'
        f'<{card}><select aria-label="Backup country"><option>Canada</option><option>Italy</option></select></{tag}>'
        "</ul></section>"
    )
    snapshot = (ROOT / "crates/fastbrowse-browser/js/snapshot.js").read_text()
    with playwright.sync_playwright() as runtime:
        browser = runtime.chromium.launch(executable_path=chrome, args=["--no-sandbox"])
        try:
            page = browser.new_page()
            page.set_content(html)
            observation = page.evaluate(snapshot + "('snapshot', null, {listeners: null})")
        finally:
            browser.close()
    contexts = {control["label"]: control.get("context") for control in observation["controls"]}
    assert contexts == {"Primary country": "Settings", "Backup country": "Settings"}, contexts
