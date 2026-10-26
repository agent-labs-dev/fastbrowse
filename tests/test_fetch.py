"""A page read over HTTP has to produce the blocks a browser capture produces, or quotes break."""

import httpx
import pytest

from fastbrowse.fetch import FetchedPage, NeedsBrowser, NotFetched, _blocks, eligible, fetch_capture
from fastbrowse.page import BlockKind

PAGE = """<!doctype html>
<html><head><title>Orders report</title>
<script>window.x = {"nested": "\\"noise\\""}</script>
<style>.a { color: red }</style></head>
<body>
<nav><a href="/">Home</a></nav>
<h1>Orders</h1>
<p>The total of the orders is <strong>251.00</strong>.</p>
<h2>Lines</h2>
<table>
  <thead><tr><th>Item</th><th>Amount</th></tr></thead>
  <tbody>
    <tr><td>Kettle</td><td>34.50</td></tr>
    <tr><td>Cups</td><td>216.50</td></tr>
  </tbody>
</table>
<ul><li>One</li><li>Two</li></ul>
<p>See the <a href="/policy">returns policy</a> for details.</p>
<a href="/download/report.csv">Download the report</a>
</body></html>"""


def transport(body: str, *, status: int = 200, content_type: str = "text/html; charset=utf-8") -> httpx.MockTransport:
    def handle(request: httpx.Request) -> httpx.Response:
        return httpx.Response(status, text=body, headers={"content-type": content_type})

    return httpx.MockTransport(handle)


def client(body: str, *, status: int = 200, content_type: str = "text/html; charset=utf-8") -> httpx.AsyncClient:
    return httpx.AsyncClient(transport=transport(body, status=status, content_type=content_type))


def test_blocks_keep_the_shapes_a_quote_needs() -> None:
    _title, blocks = _blocks(PAGE, "https://shop.example.com/orders")
    texts = [(block.kind, block.text) for block in blocks]

    assert (BlockKind.HEADING, "Orders") in texts
    # A value inside markup stays in the block of its label, which is what names it.
    assert (
        BlockKind.PARAGRAPH,
        "The total of the orders is 251.00.",
    ) in texts
    # Scripts state nothing a reader can quote, and a CSS rule is not page text.
    assert not any("window.x" in text or "color: red" in text for _kind, text in texts)
    # Each row holds its cells in one line, under the header that names them.
    assert (BlockKind.TABLE, "| Item | Amount |") in texts
    assert (BlockKind.TABLE, "| --- | --- |") in texts
    assert (BlockKind.TABLE, "| Kettle | 34.50 |") in texts
    assert (BlockKind.LIST_ITEM, "One") in texts
    # A link's destination is kept where the link is the whole block.
    assert (BlockKind.PARAGRAPH, "See the returns policy for details.") in texts
    links = [block for block in blocks if block.kind is BlockKind.LINK]
    assert [(block.text, block.href) for block in links] == [
        ("Home", "https://shop.example.com/"),
        ("Download the report", "https://shop.example.com/download/report.csv"),
    ]


def test_a_value_under_a_heading_carries_the_path_that_names_it() -> None:
    _title, blocks = _blocks(PAGE, "https://shop.example.com/orders")
    row = next(block for block in blocks if block.text == "| Kettle | 34.50 |")
    assert row.heading_path == ("Orders", "Lines")


async def test_a_fetched_page_is_a_capture_with_spans_and_a_hash() -> None:
    async with client(PAGE) as http:
        capture = await fetch_capture(http, "https://shop.example.com/orders")

    assert capture.title == "Orders report"
    for block in capture.blocks:
        assert capture.text[block.start : block.end] == capture.text[block.start : block.end]
        assert block.end > block.start
    assert "The total of the orders is 251.00." in capture.text
    # A quote is validated against this hash, so it has to be the hash of exactly this text.
    import hashlib

    assert capture.sha256 == hashlib.sha256(capture.text.encode()).hexdigest()


async def test_plain_text_is_read_as_one_block() -> None:
    async with client("total,amount\nkettle,34.50\n", content_type="text/plain") as http:
        capture = await fetch_capture(http, "https://shop.example.com/report.txt")
    assert [block.kind for block in capture.blocks] == [BlockKind.PARAGRAPH]


async def test_a_page_that_is_not_html_is_left_to_the_browser() -> None:
    async with client("{}", content_type="application/json") as http:
        with pytest.raises(NotFetched, match="not a document read as text"):
            await fetch_capture(http, "https://shop.example.com/api")
    async with client("gone", status=404) as http:
        with pytest.raises(NotFetched, match="HTTP 404"):
            await fetch_capture(http, "https://shop.example.com/missing")


async def test_a_shell_or_a_wall_needs_the_browser_that_renders_it() -> None:
    shell = "<html><body><div id=root></div></body></html>" + "<script>a=1</script>" * 200
    async with client(shell) as http:
        with pytest.raises(NotFetched, match="scripts"):
            await fetch_capture(http, "https://shop.example.com/app")
    wall = "<html><body><h1>Just a moment</h1><p>Checking your browser</p></body></html>"
    async with client(wall) as http:
        with pytest.raises(NotFetched, match="bot check"):
            await fetch_capture(http, "https://shop.example.com/app")


def test_an_address_is_refused_by_what_it_would_hide() -> None:
    with pytest.raises(NotFetched, match="credentials"):
        eligible("https://user:pass@shop.example.com/orders", None)
    with pytest.raises(NotFetched, match="not an address"):
        eligible("file:///etc/passwd", None)
    with pytest.raises(NotFetched, match="outside the origins"):
        eligible("https://elsewhere.example.com/orders", ("https://shop.example.com",))
    eligible("https://shop.example.com/orders", ("https://shop.example.com",))


async def test_a_redirect_outside_the_grant_is_refused() -> None:
    def handle(request: httpx.Request) -> httpx.Response:
        if request.url.host == "shop.example.com":
            return httpx.Response(302, headers={"location": "https://elsewhere.example.com/orders"})
        return httpx.Response(200, text="<p>elsewhere</p>", headers={"content-type": "text/html"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handle)) as http:
        with pytest.raises(NotFetched, match="outside the origins"):
            await fetch_capture(http, "https://shop.example.com/orders", allowed_origins=("https://shop.example.com",))


async def test_a_fetched_page_reads_but_cannot_act() -> None:
    async with client(PAGE) as http:
        page = FetchedPage(http)
        await page.navigate("https://shop.example.com/orders")
        observation = await page.observe()

        assert observation.url == "https://shop.example.com/orders"
        assert observation.controls == ()
        assert observation.viewport_text
        assert await page.address() == observation.url
        capture = await page.capture()
        assert "| Kettle | 34.50 |" in capture.text
        assert await page.document_changed(observation) is False

        # A step that would act has nothing to act on, and a run that pretends otherwise answers from a page
        # it never saw.
        from fastbrowse.models import Operation
        from fastbrowse.page import Action

        with pytest.raises(NeedsBrowser, match="no controls"):
            await page.act(Action(operation=Operation.CLICK, target_id="1"), observation)
        with pytest.raises(NeedsBrowser, match="no pixels"):
            await page.screenshot()


def test_a_class_a_rule_hides_is_not_read() -> None:
    page = """<html><head><style>.gone { display: none } .kept { color: red }</style></head>
    <body><p>Shown price: 10.00</p><p class="gone">Old price: 99.00</p><p class="kept">Note: read</p></body></html>"""
    _title, blocks = _blocks(page, "https://shop.example.com/orders")
    texts = [block.text for block in blocks]

    assert "Shown price: 10.00" in texts
    # A template or a stale value the page does not display is not evidence a reader can quote.
    assert not any("99.00" in text for text in texts)
    assert any("Note: read" in text for text in texts)


async def test_a_genuinely_short_document_is_read() -> None:
    page = "<html><body><h1>Status</h1><p>All systems are up.</p></body></html>"
    async with client(page) as http:
        capture = await fetch_capture(http, "https://status.example.com/")
    assert "All systems are up." in capture.text


async def test_a_body_past_the_size_bound_is_refused_as_it_arrives() -> None:
    async with client("<html><body>" + "a" * (6 * 1024 * 1024) + "</body></html>") as http:
        with pytest.raises(NotFetched, match="past"):
            await fetch_capture(http, "https://shop.example.com/huge")


async def test_access_withdrawn_after_the_navigation_stops_the_read() -> None:
    allowed = True

    async def gate() -> None:
        if not allowed:
            raise RuntimeError("withdrawn")

    async with client(PAGE) as http:
        page = FetchedPage(http, check_access=gate)
        await page.navigate("https://shop.example.com/orders")
        assert await page.observe() is not None

        allowed = False
        with pytest.raises(RuntimeError, match="withdrawn"):
            await page.observe()
        with pytest.raises(RuntimeError, match="withdrawn"):
            await page.capture()
