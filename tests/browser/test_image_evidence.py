"""Rendered image elements need DOM evidence even when they contain no text."""

from fastbrowse.browser import BrowserSession, CdpPage
from tests.browser.test_browser import eval_value


async def test_capture_reports_visible_image_metadata_without_inventing_pixels(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<h1>README</h1><a href="/map.png">' +
          '<img src="/map.png" alt="Minimap screenshot"></a>' +
          '<img src="/hidden.png" alt="Private image" style="display:none">';
    """,
    )
    capture = await page.capture()
    assert "Minimap screenshot" in capture.text
    assert "/map.png" in capture.text
    assert "Private image" not in capture.text
    assert "Image element" in capture.text
