"""Semantic reply records keep their owner when sibling records use different tags."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


@pytest.mark.parametrize("owner_tag", ["h3", "b"])
def test_mixed_semantic_reply_records_keep_each_counter_with_its_author(owner_tag: str) -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    html = (
        "<section><h2>Comments</h2>"
        f"<article><{owner_tag}>Ada</{owner_tag}><p>Parent comment.</p>"
        "<button>Reply</button><button>Hide 2 Replies</button>"
        '<div role="list"><article role="listitem"><b>Grace</b><p>A reply.</p>'
        "<button>Reply</button><button>Hide 3 Replies</button></article>"
        '<div role="listitem"><b>Linus</b><p>Another reply.</p>'
        "<button>Reply</button><button>Hide 4 Replies</button></div></div></article>"
        "<article><h3>Ruth</h3><p>Another comment.</p>"
        "<button>Reply</button><button>Hide 5 Replies</button></article></section>"
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
    assert replies == ["Ada", "Grace", "Linus", "Ruth"]
    counters = {
        control["label"]: control.get("context")
        for control in observation["controls"]
        if control["label"] in {"Hide 3 Replies", "Hide 4 Replies"}
    }
    assert counters == {"Hide 3 Replies": "Grace", "Hide 4 Replies": "Linus"}, counters
