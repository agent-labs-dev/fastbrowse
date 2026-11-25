"""Records directly inside an open shadow root are listed by that root, as records in a document are by their parent."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


def observe_shadow(html: str) -> dict:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
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
            page.set_content("<div id=host></div>")
            page.eval_on_selector("#host", "(host, html) => host.attachShadow({mode: 'open'}).innerHTML = html", html)
            # The fixture has native controls only, so it needs no script listener discovery.
            return page.evaluate(snapshot + "('snapshot', null, {listeners: null})")
        finally:
            browser.close()


# A record at a shadow root has no parent element, and reading its siblings through one threw and lost the whole page.
@pytest.mark.parametrize("record", ["article", "li", 'div role="listitem"'])
def test_shadow_root_record_keeps_its_comment_form_outside_its_comments(record: str) -> None:
    tag = record.split()[0]
    observation = observe_shadow(
        f'<{record} aria-label="Post">'
        '<textarea aria-label="New comment"></textarea><ul>'
        "<li><b>Ada</b><p>Ada's comment.</p><button>Reply</button></li>"
        "<li><b>Grace</b><p>Grace's comment.</p><button>Reply</button></li>"
        f"</ul></{tag}>"
    )
    editor = next(control for control in observation["controls"] if control["label"] == "New comment")
    assert editor.get("context") == "Post, outside every item listed under Post"


# Comments listed straight under a shadow root repeat among the root's children; counted by a parent element, they had
# no siblings, and each comment's "Hide N Replies" counter lost the comment it belongs to.
def test_shadow_root_comments_name_their_own_counters() -> None:
    observation = observe_shadow(
        "<h2>Comments</h2>"
        + "".join(
            f"<li><div class=comment><b>{author}</b><p>Text by {author}.</p><button>Reply</button></div>"
            f"<button>Hide {count} Replies</button></li>"
            for author, count in [("Ada", 3), ("Grace", 4)]
        )
    )
    replies = [control.get("context") for control in observation["controls"] if control["label"] == "Reply"]
    counters = [control.get("context") for control in observation["controls"] if control["label"].startswith("Hide ")]
    assert replies == ["Ada", "Grace"]
    assert counters == replies
