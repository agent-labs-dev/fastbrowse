"""Capture blocks preserve the records a reader needs to cite together."""

import json

import pytest

from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.page import BlockKind
from tests.browser.test_browser import eval_value, wait_until


async def test_quote_cards_are_whole_records_with_local_titles(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    capture = await page.capture()
    records = [
        block
        for block in capture.blocks
        if block.kind is BlockKind.RECORD and block.heading_path == ("Fastbrowse fixture", "Quotations")
    ]
    expected = [
        ("Measure twice, cut once.", "by Ada (about)", "craft care"),
        ("Leave room for a second idea.", "by Grace (about)", "ideas work"),
        ("Ask what the evidence says.", "by Katherine (about)", "science care"),
    ]
    assert len(records) == len(expected)
    for block, (quotation, author, tags) in zip(records, expected, strict=True):
        text = capture.text[block.start : block.end]
        assert f"{quotation}\n" in text and f"{author}\nTags: {tags}" in text
        assert block.heading_path == ("Fastbrowse fixture", "Quotations")
        assert block.frame_id is None and block.source_id.startswith("main/:")
        assert block.href is None
        assert capture.text.count(quotation) == capture.text.count(author) == 1
    assert "A thought for today" in capture.text[records[-1].start : records[-1].end]
    after = next(block for block in capture.blocks if capture.text[block.start : block.end] == "After the quotations.")
    assert after.heading_path == ("Fastbrowse fixture", "Quotations")
    standalone = [block for block in capture.blocks if "A standalone" in capture.text[block.start : block.end]]
    assert [capture.text[block.start : block.end] for block in standalone] == [
        "A standalone first paragraph.",
        "A standalone second paragraph.",
    ]
    assert all(block.kind is BlockKind.PARAGRAPH for block in standalone)


async def test_each_table_row_repeats_its_header(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    capture = await page.capture()
    tables = [block for block in capture.blocks if block.kind is BlockKind.TABLE]
    assert [capture.text[block.start : block.end] for block in tables] == [
        "| Name | Score |\n| --- | --- |\n| Ada | 10 |",
        "| Name | Score |\n| --- | --- |\n| Grace | 9 |",
        "| Team | Points |\n| --- | --- |\n| North | 12 |",
        "| Team | Points |\n| --- | --- |\n| East | 11 |",
        "| Team | Points |\n| --- | --- |\n| West | 10 |",
    ]
    assert [block.heading_path for block in tables] == [
        ("Fastbrowse fixture", "Table"),
        ("Fastbrowse fixture", "Table"),
        ("Fastbrowse fixture", "Rankings"),
        ("Fastbrowse fixture", "Rankings"),
        ("Fastbrowse fixture", "Rankings"),
    ]
    assert len({block.source_id for block in tables}) == 5


@pytest.mark.parametrize(
    ("rows", "expected"),
    [
        (
            "<thead><tr><td>Group</td></tr><tr><th>Name</th></tr></thead>"
            "<tbody><tr><td>A | B</td></tr><tr><td>C</td></tr></tbody>",
            ["| Group |\n| Name |\n| --- |\n| A \\| B |", "| Group |\n| Name |\n| --- |\n| C |"],
        ),
        ("<tr><td>A</td><td>1</td></tr><tr><th>B</th><td>2</td></tr>", ["| A | 1 |", "| B | 2 |"]),
        ("<thead><tr><th>Name</th></tr><tr><th>Person</th></tr></thead>", ["| Name |\n| Person |\n| --- |"]),
        ("<tr><th>Name | alias</th></tr>", ["| Name \\| alias |\n| --- |"]),
        (
            "<thead><tr hidden><th>Group</th></tr><tr><th>Name</th></tr></thead>"
            "<tbody><tr style='display:none'><td>X</td></tr><tr><td>A</td></tr>"
            "<tr style='visibility:collapse'><td>Y</td></tr></tbody>"
            "<tfoot><tr style='visibility:hidden'><td>Z</td></tr><tr><td>Total</td></tr></tfoot>",
            ["| Name |\n| --- |\n| A |", "| Name |\n| --- |\n| Total |"],
        ),
        (
            "<thead style='display:none'><tr><th>Name</th></tr></thead><tbody><tr><td>A</td></tr></tbody>"
            "<tfoot style='visibility:collapse'><tr><td>Total</td></tr></tfoot>",
            ["| A |"],
        ),
        (
            "<tr><th>Name</th></tr><tr hidden><td>Ada</td></tr><tr style='display:none'><td>Grace</td></tr>",
            ["| Name |\n| --- |"],
        ),
        ("<tr hidden><td>Ada</td></tr>", []),
        (
            "<tr><th>Name</th><th style='display:none'>Id</th></tr>"
            "<tr><td>Ada</td><td style='display:none'>7</td></tr>",
            ["| Name |\n| --- |\n| Ada |"],
        ),
        (
            "<tr><th>Name</th><th>Id</th><th>Team</th></tr>"
            "<tr><td>Ada</td><td style='visibility:hidden'>7</td><td>North</td></tr>",
            ["| Name | Id | Team |\n| --- | --- | --- |\n| Ada |  | North |"],
        ),
    ],
    ids=[
        "multiple-header-rows",
        "no-header",
        "header-only",
        "leading-header-only",
        "hidden-rows",
        "hidden-sections",
        "every-data-row-hidden",
        "every-row-hidden",
        "hidden-column",
        "invisible-cell-keeps-its-column",
    ],
)
async def test_table_header_variants(
    page: CdpPage, browser_session: BrowserSession, main_site: str, rows: str, expected: list[str]
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        f"document.body.innerHTML = {json.dumps(f'<table>{rows}</table>')}",
    )
    capture = await page.capture()
    assert [capture.text[block.start : block.end] for block in capture.blocks] == expected
    assert all(block.kind is BlockKind.TABLE for block in capture.blocks)


async def test_table_rows_below_the_viewport_are_captured(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = '<table style="margin-top:200vh"><tr><td>Below the viewport</td></tr></table>'
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    assert await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelector('tr').getBoundingClientRect().top > innerHeight",
    )
    capture = await page.capture()
    assert [capture.text[block.start : block.end] for block in capture.blocks] == ["| Below the viewport |"]
    assert capture.text == "| Below the viewport |\n\n"
    assert all(block.kind is BlockKind.TABLE for block in capture.blocks)


@pytest.mark.parametrize(("tag", "kind"), [("div", BlockKind.RECORD), ("li", BlockKind.LIST_ITEM)])
async def test_records_keep_single_links_and_a_page_sized_card_is_not_one_record(
    page: CdpPage, browser_session: BrowserSession, main_site: str, tag: str, kind: BlockKind
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    cards = "".join(
        f'<{tag} class="card"><div>{"x" * length}</div><a href="/author">Author</a></{tag}>'
        for length in (40, 40, 5000)
    )
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        f"document.body.innerHTML = {json.dumps(f'<ul>{cards}</ul>')}",
    )
    capture = await page.capture()
    assert [block.kind for block in capture.blocks] == [kind, kind, BlockKind.PARAGRAPH, BlockKind.LINK]
    assert all(block.href == "/author" for block in capture.blocks[:2])


@pytest.mark.parametrize(
    "cards",
    [
        "<div class='card'><p>First</p><p>Second</p></div>" * 2,
        '<div class="one"><p>First</p></div><div class="two"><p>Second</p></div>'
        '<div class="one two"><p>Third</p></div>',
        "<div><p>First</p></div><section><p>Second</p></section><article><p>Third</p></article>",
        "<div class='card'><span>Inline only</span></div>" * 3,
        "<div class='card'><h2>First title</h2><h3>Second title</h3><p>Body</p></div>" * 3,
        "<div class='card'><p>Outside</p><table><tr><td>Inside</td></tr></table></div>" * 3,
        "<div class='card'><p>Outside</p><ul><li>Inside</li><li>Another</li><li>Last</li></ul></div>" * 3,
        "<div><p>Layout</p><p>column</p></div>" * 3,
    ],
    ids=[
        "two-siblings",
        "different-classes",
        "different-tags",
        "inline-only",
        "two-headings",
        "table",
        "list",
        "classless",
    ],
)
async def test_non_records_fall_back_to_the_walk(
    page: CdpPage, browser_session: BrowserSession, main_site: str, cards: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(cards)}"
    )
    capture = await page.capture()
    assert capture.blocks
    assert all(block.kind is not BlockKind.RECORD for block in capture.blocks)


def _rated_card(rating: str, price: str) -> str:
    # books.toscrape.com: every rating draws the same five icons, whatever the class says; only the class or an
    # accessible label ever states which one it is.
    stars = "".join('<i class="icon-star"></i>' for _ in range(5))
    return f'<article class="card"><h3>Book</h3><p class="star-rating {rating}">{stars}</p><p>{price}</p></article>'


async def test_a_class_only_star_rating_reaches_the_record(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = _rated_card("Five", "£12.51") + _rated_card("Three", "£9.99") + _rated_card("Five", "£4.00")
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    records = [block for block in capture.blocks if block.kind is BlockKind.RECORD]
    assert len(records) == 3
    texts = [capture.text[block.start : block.end] for block in records]
    assert "Five stars" in texts[0] and "£12.51" in texts[0]
    assert "Three stars" in texts[1] and "£9.99" in texts[1]
    assert "Five stars" in texts[2]


async def test_an_accessible_rating_label_reaches_the_text(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = '<p>Great product <span role="img" aria-label="4 out of 5 stars"></span></p>'
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    assert any("4 out of 5 stars" in capture.text[block.start : block.end] for block in capture.blocks)


async def test_a_hidden_rating_widget_adds_nothing(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = '<p>Item</p><p class="star-rating Five" hidden></p>'
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    assert not any("stars" in capture.text[block.start : block.end] for block in capture.blocks)


def _card_with_hidden_wrapper() -> str:
    # A hidden wrapper (e.g. a template div a site keeps offscreen) holds a five-star widget; a visible
    # one-star widget follows. The hidden widget's own `hidden` check on itself passes (it isn't hidden),
    # so only checking ancestors up to the record root stops it from winning the descendant search.
    return (
        '<article class="card"><h3>Book</h3>'
        '<div hidden><p class="star-rating Five"></p></div>'
        '<p class="star-rating One"></p></article>'
    )


async def test_a_rating_hidden_by_an_ancestor_is_ignored(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = _card_with_hidden_wrapper() + _rated_card("Two", "£1.00") + _rated_card("Three", "£2.00")
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    records = [block for block in capture.blocks if block.kind is BlockKind.RECORD]
    assert len(records) == 3
    first = capture.text[records[0].start : records[0].end]
    assert "Five stars" not in first
    assert "One stars" in first


def _card_with_invisible_wrapper() -> str:
    # A wrapper hidden via CSS `visibility:hidden` (inherited by its children, unlike `display:none`) holds
    # a five-star widget; a visible one-star widget follows. ratingOf's own `hidden()` check only looks at
    # `hidden`/`display`, so the invisible five-star widget must be rejected by its own computed visibility.
    return (
        '<article class="card"><h3>Book</h3>'
        '<div style="visibility:hidden"><p class="star-rating Five"></p></div>'
        '<p class="star-rating One"></p></article>'
    )


async def test_a_rating_hidden_by_ancestor_visibility_is_ignored(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    markup = _card_with_invisible_wrapper() + _rated_card("Two", "£1.00") + _rated_card("Three", "£2.00")
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    records = [block for block in capture.blocks if block.kind is BlockKind.RECORD]
    assert len(records) == 3
    first = capture.text[records[0].start : records[0].end]
    assert "Five stars" not in first
    assert "One stars" in first


async def test_an_arbitrary_class_naming_a_number_is_not_read_as_a_rating(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    # Only the literal `star-rating` class is trusted; a differently named class that happens to carry a rating
    # word, or a plain icon count, must never be inferred as one.
    await page.navigate(f"{main_site}/icons.html")
    markup = '<p class="badge Five">Featured</p>'
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    assert not any("stars" in capture.text[block.start : block.end] for block in capture.blocks)


async def test_nested_record_lists_keep_the_inner_records(page: CdpPage, browser_session: BrowserSession) -> None:
    await page.navigate("about:blank")
    cards = "<article><p>Quotation</p><p>by Author</p></article>" * 3
    markup = f"<div><p>Collection</p><section>{cards}</section></div>" * 3
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(markup)}"
    )
    capture = await page.capture()
    records = [block for block in capture.blocks if block.kind is BlockKind.RECORD]
    assert len(records) == 9
    assert all(capture.text[block.start : block.end] == "Quotation\n\nby Author" for block in records)


@pytest.mark.parametrize("boundary", ["iframe", "shadow-host", "shadow-descendant"])
async def test_record_candidates_preserve_frame_and_shadow_boundaries(
    page: CdpPage, browser_session: BrowserSession, boundary: str
) -> None:
    await page.navigate("about:blank")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = '<article><p>Outer first</p><p>Outer second</p></article>'.repeat(3);
        for (const card of document.querySelectorAll('article')) {
            const boundary = """
        + json.dumps(boundary)
        + """;
            const text = '<p>Inner first</p><p>Inner second</p>';
            if (boundary === 'iframe') {
                const frame = document.createElement('iframe');
                frame.srcdoc = text;
                card.append(frame);
            } else {
                const host = boundary === 'shadow-host' ? card : card.appendChild(document.createElement('div'));
                host.attachShadow({mode: 'open'}).innerHTML = '<slot></slot>' + text;
            }
        }""",
    )

    async def frames_loaded() -> bool:
        return await eval_value(
            browser_session,
            browser_session.active_session_id,
            "[...document.querySelectorAll('iframe')].every("
            "f => f.contentDocument?.body?.innerText.includes('Inner first'))",
        )

    await wait_until(frames_loaded)
    capture = await page.capture()
    assert capture.inaccessible_frames == 0
    assert all(block.kind is BlockKind.PARAGRAPH for block in capture.blocks)
    inner = [block for block in capture.blocks if capture.text[block.start : block.end] == "Inner first"]
    assert len(inner) == 3
    if boundary == "iframe":
        assert all(block.frame_id for block in inner)
        assert len({block.frame_id for block in inner}) == 3
    else:
        assert all(block.frame_id is None and "/shadow:" in block.source_id for block in inner)
        assert len({block.source_id.rsplit(":", 1)[0] for block in inner}) == 3


async def test_a_capture_waits_for_a_loading_indicator_an_action_gave_up_on(page: CdpPage, main_site: str) -> None:
    """A read of "Loading..." costs an LLM call and a second read once the page has drawn what it was loading."""
    await page.navigate(f"{main_site}/loading.html")
    capture = await page.capture()
    assert "Hello World!" in capture.text
    assert "Loading..." not in capture.text
