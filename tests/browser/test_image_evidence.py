"""Rendered image elements need DOM evidence even when they contain no text."""

from fastbrowse.browser import BrowserSession, CdpPage
from tests.browser.test_browser import eval_value


async def test_a_figure_is_visual_content_but_an_icon_sized_drawing_is_not(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """The verifier needs a screenshot when the page's state is a picture, but an icon is not page state, so
    the observation reports a figure and ignores a drawing too small to carry anything."""
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<canvas width="320" height="240"></canvas>';
        """,
    )
    assert (await page.observe()).visual_content
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<canvas width="16" height="16"></canvas>';
        """,
    )
    assert not (await page.observe()).visual_content


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


async def test_capture_does_not_copy_inline_image_payload(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<img alt="Inline chart">';
        document.querySelector('img').src = 'data:image/png;base64,' + 'PAYLOAD'.repeat(100000);
        """,
    )
    capture = await page.capture()
    assert "Inline chart" in capture.text
    assert "data:image/png;base64,[payload omitted]" in capture.text
    assert "PAYLOAD" not in capture.text
    assert len(capture.text) < 2000


async def test_image_observations_are_bounded_and_ignore_decode_state(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = Array.from({length: 300}, (_, i) =>
          `<img alt="Chart ${i}" src="blob:${'x'.repeat(4000)}">`).join('');
        """,
    )
    before = await page.capture()
    assert before.text.count("Image element DOM metadata") == 32
    assert "blob:[identifier omitted]" in before.text
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        for (const image of document.querySelectorAll('img')) {
          Object.defineProperty(image, 'complete', {value: true});
          Object.defineProperty(image, 'naturalWidth', {value: 100});
        }
        """,
    )
    assert (await page.capture()).text == before.text
