"""A link drawn only as an icon says where it goes and nothing else, so its destination must reach the capture."""

from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.page import BlockKind
from tests.browser.test_browser import eval_value


async def test_capture_reports_icon_link_destination_with_its_row(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<h1>Releases</h1><div class="row"><span>v2.1.0</span> <span>12 KB</span> ' +
          '<span><a href="/files/abc.zip?sig=1"><svg viewBox="0 0 8 8"><path d="M0 0h8v8z"></path></svg></a>' +
          '</span></div><a href="/about">About</a>' +
          '<a href="/private.zip" style="display:none"><svg></svg></a><a href="#menu"><svg></svg></a>';
    """,
    )
    capture = await page.capture()
    destination = f"{main_site.rstrip('/')}/files/abc.zip?sig=1"
    observed = [
        capture.text[block.start : block.end]
        for block in capture.blocks
        if block.kind is BlockKind.OBSERVATION and destination in capture.text[block.start : block.end]
    ]
    assert len(observed) == 1
    assert "v2.1.0 12 KB" in observed[0]
    # A link with words says what it is in the text already; a hidden one is not on the page a user sees.
    assert "/about" not in capture.text
    assert "/private.zip" not in capture.text
    assert "#menu" not in capture.text


async def test_icon_link_observations_are_bounded(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = Array.from({length: 100}, (_, i) =>
          `<p>Row ${i} <a href="/f/${i}?k=${'x'.repeat(4000)}"><svg></svg></a></p>`).join('');
        """,
    )
    capture = await page.capture()
    assert capture.text.count("Link element DOM metadata") == 32
    assert "Additional link destinations omitted." in capture.text
    assert len(capture.text) < 100_000
