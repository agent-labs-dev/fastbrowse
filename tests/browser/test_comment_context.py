"""Reply controls belong to their comment record, including when only one form is open."""

import pytest

from fastbrowse.browser.page import CdpPage


@pytest.mark.parametrize("record", ["li", "article", 'div role="listitem"'])
async def test_one_open_reply_form_does_not_inherit_the_first_commenter(page: CdpPage, record: str) -> None:
    tag = record.split()[0]
    await page.navigate(
        "data:text/html,<form><textarea aria-label='Comment text area'></textarea><button>Post</button></form>"
        "<section><h2>Comments</h2><ul>"
        f"<{record}><a>First commenter</a><p>First request</p><button>Reply</button></{tag}>"
        f"<{record}><a>Merlinqt</a><p>Scale map icons</p><button>Reply</button>"
        "<form><textarea aria-label='Comment text area'>@Merlinqt </textarea>"
        f"<p>Please follow our Terms of Service</p><button>Cancel</button><button>Post</button></form></{tag}>"
        "</ul></section>"
    )
    observation = await page.observe()
    field = next(c for c in observation.controls if c.label == "Comment text area" and c.value == "@Merlinqt ")
    posts = [c for c in observation.controls if c.label == "Post"]
    assert field.context == "Merlinqt"
    assert [c.context for c in posts] == [None, "Merlinqt"]


@pytest.mark.parametrize("hidden_reply", [False, True])
async def test_counted_collection_retains_every_visible_member(page: CdpPage, hidden_reply: bool) -> None:
    from fastbrowse.memory import CollectionFact, Notes
    from fastbrowse.retrieval import read
    from tests.test_retrieval import ScriptedLLM

    style = ' style="display:none"' if hidden_reply else ""
    await page.navigate(
        "data:text/html,<h1>Birch</h1><section><h2>Comments (3)</h2><ul>"
        "<li><p>Elm: Please add search.</p><button>Reply</button><ul>"
        f"<li{style}><p>Author Ash: Added.</p><button>Reply</button></li></ul></li>"
        "<li><p>Oak: Thanks.</p><button>Reply</button></li></ul></section><footer>Footer links</footer>"
    )
    captured = await page.capture()
    notes = Notes()
    await read(
        ScriptedLLM([{"claims": [], "answered": False, "collections": []}]),
        captured,
        "Report author replies for Birch.",
        ["r"],
        notes,
        preserve_collections=True,
        field_outputs=True,
    )
    collections = [f for f in notes.facts if isinstance(f, CollectionFact)]
    assert bool(collections) is (not hidden_reply)
    if collections:
        assert collections[0].comparison.complete
        quotes = [notes.evidence[key].quote for key in collections[0].comparison.records]
        assert any("Author Ash: Added." in quote for quote in quotes)
        assert not any("Footer" in quote for quote in quotes)


@pytest.mark.parametrize("inside", [False, True])
async def test_a_list_records_whether_its_element_holds_a_frame(page: CdpPage, inside: bool) -> None:
    frame = "<iframe sandbox srcdoc='<p>Advertisement</p>'></iframe>"
    await page.navigate(
        "data:text/html,<h1>Birch</h1>"
        + ("" if inside else frame)
        + "<h2>Comments (2)</h2><ul><li><p>Elm: The map crashes.</p></li><li><p>Oak: Thanks.</p>"
        + (frame if inside else "")
        + "</li></ul>"
    )
    captured = await page.capture()
    members = [block for block in captured.blocks if block.list_id is not None]
    assert members
    assert {block.list_frameless for block in members} == {not inside}


async def test_a_list_that_hosts_a_shadow_root_is_not_frameless(page: CdpPage) -> None:
    await page.navigate(
        "data:text/html,<h1>Birch</h1><h2>Comments (2)</h2>"
        "<div id=list role=list><div role=listitem><p>Elm: The map crashes.</p></div>"
        "<div role=listitem><p>Oak: Thanks.</p></div></div>"
        "<script>document.getElementById('list').attachShadow({mode: 'open'}).innerHTML ="
        " \"<slot></slot><iframe sandbox srcdoc='<p>More</p>'></iframe>\";</script>"
    )
    captured = await page.capture()
    members = [block for block in captured.blocks if block.list_id is not None]
    assert members
    assert {block.list_frameless for block in members} == {False}
