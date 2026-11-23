"""A comment's layout rows must not replace its author as a counter's context."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


def test_reply_counters_keep_their_comment_names_past_repeated_layout_rows() -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    html = (
        "<section><h2>Comments</h2><ul>"
        + "".join(
            f'<li><h3>{author}</h3><div class="row">Text by {author}.</div>'
            f'<div class="row"><p>Actions</p><button>Reply</button>'
            f"<button>Hide {count} Replies</button></div></li>"
            for author, count in [("Ada", 3), ("Grace", 4)]
        )
        + "</ul></section>"
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
            # The fixture has native buttons only, so it needs no script listener discovery.
            observation = page.evaluate(snapshot + "('snapshot', null, {listeners: null})")
        finally:
            browser.close()
    replies = [control.get("context") for control in observation["controls"] if control["label"] == "Reply"]
    counters = [control.get("context") for control in observation["controls"] if control["label"].startswith("Hide ")]
    assert replies == ["Ada", "Grace"]
    assert counters == replies, f"Counters lost their comment owners: {counters}"
