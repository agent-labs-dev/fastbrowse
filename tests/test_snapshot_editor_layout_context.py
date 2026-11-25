"""A page-level comment form laid out in repeated rows still sits outside every comment."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


def test_editor_layout_rows_keep_the_note_that_they_are_outside_the_comments() -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    html = (
        "<section><h2>Comments</h2><form>"
        '<div class="row"><textarea aria-label="Comment text area"></textarea></div>'
        '<div class="row"><button>Post comment</button></div></form><ul>'
        "<li><h3>Ada</h3><p>Ada's comment.</p><button>Reply</button></li>"
        "<li><h3>Grace</h3><p>Grace's comment.</p><button>Reply</button></li>"
        "</ul></section>"
    )
    snapshot = (ROOT / "crates/fastbrowse-browser/js/snapshot.js").read_text()
    with playwright.sync_playwright() as runtime:
        browser = runtime.chromium.launch(
            executable_path=chrome,
            args=[
                "--no-sandbox",
                "--no-zygote",
                "--single-process",
                "--in-process-gpu",
                "--use-gl=angle",
                "--use-angle=swiftshader",
                "--enable-unsafe-swiftshader",
            ],
        )
        try:
            page = browser.new_page()
            page.set_content(html)
            observation = page.evaluate(snapshot + "('snapshot', null, {listeners: null})")
        finally:
            browser.close()
    contexts = {
        control["label"]: control.get("context")
        for control in observation["controls"]
        if control["label"] in {"Comment text area", "Post comment"}
    }
    outside = "Comments, outside every item listed under Comments"
    assert contexts == {"Comment text area": outside, "Post comment": outside}, contexts
