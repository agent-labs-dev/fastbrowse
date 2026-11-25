"""An author link in a layout row still names the comment's reply counter."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


@pytest.mark.parametrize("card", ["li", "div class=comment-card"])
def test_author_link_in_a_sibling_layout_row_names_the_counter(card: str) -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    tag = card.split()[0]
    html = (
        "<section><h2>Comments</h2><ul>"
        + "".join(
            f'<{card}><div class="row"><a href="#{author}">{author}</a></div>'
            f'<p>Text by {author}.</p><div class="row"><p>Actions</p><button>Reply</button>'
            f"<button>Hide {count} Replies</button></div></{tag}>"
            for author, count in [("Ada", 3), ("Grace", 4)]
        )
        + "</ul></section>"
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
    replies = [control.get("context") for control in observation["controls"] if control["label"] == "Reply"]
    counters = [control.get("context") for control in observation["controls"] if control["label"].startswith("Hide ")]
    assert replies == ["Ada", "Grace"]
    assert counters == replies, f"Counters lost their comment owners to layout rows: {counters}"
