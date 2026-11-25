"""A parent comment's heading must not rename a lone reply's controls."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


def test_lone_reply_keeps_its_name_when_parent_comment_has_a_heading() -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    html = (
        "<section><h2>Comments</h2><ul>"
        "<li><h3>Ada</h3><p>Parent comment.</p><button>Reply</button><button>Hide 1 Replies</button>"
        "<ul><li><b>Grace</b><p>A reply to Ada.</p><button>Reply</button>"
        "<button>Hide 3 Replies</button></li></ul></li>"
        "<li><h3>Linus</h3><p>Another comment.</p><button>Reply</button>"
        "<button>Hide 4 Replies</button></li></ul></section>"
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
    replies = [control.get("context") for control in observation["controls"] if control["label"] == "Reply"]
    counter = next(control for control in observation["controls"] if control["label"] == "Hide 3 Replies")
    assert replies == ["Ada", "Grace", "Linus"]
    assert counter.get("context") == "Grace", f"Grace's counter is named {counter.get('context')!r}"
