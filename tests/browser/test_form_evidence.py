from fastbrowse.browser import CdpPage
from fastbrowse.models import BrowserConnection
from tests.browser.test_browser import eval_value, observe_until
from tests.browser.test_safety import scoped


async def test_capture_preserves_empty_fields_disabled_actions_and_layout(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await eval_value(
        page._session,
        page._session.active_session_id,
        "document.body.innerHTML = '<label>Name<input></label><button disabled>Create</button>'",
    )
    capture = await page.capture()
    assert "Name: [empty]" in capture.text
    assert "Create: disabled" in capture.text
    assert "Horizontal overflow: no" in capture.text


async def test_capture_evidences_absent_visible_headings(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await eval_value(
        page._session,
        page._session.active_session_id,
        "document.body.innerHTML = '<h1 hidden>Hidden</h1><p>Welcome</p>'",
    )
    capture = await page.capture()
    assert "Visible h1 headings: 0" in capture.text


async def test_heading_absence_does_not_ignore_shadow_roots(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await eval_value(
        page._session,
        page._session.active_session_id,
        "document.body.innerHTML = '<div id=host></div>'; "
        "document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<h1>Visible title</h1>'",
    )
    capture = await page.capture()
    assert "Visible title" in capture.text
    assert "Visible h1 headings: 0" not in capture.text


async def test_empty_shadow_scope_does_not_contradict_document_heading(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await eval_value(
        page._session,
        page._session.active_session_id,
        "document.body.innerHTML = '<h1>Visible title</h1><div id=host></div>'; "
        "document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<p>Other content</p>'",
    )
    capture = await page.capture()
    assert "Visible h1 headings: 0" not in capture.text


async def test_heading_absence_includes_cross_origin_frame_documents(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await observe_until(page, "Frame button")
    await eval_value(
        page._session,
        page._session.active_session_id,
        "document.querySelectorAll('h1').forEach(e => e.remove())",
    )
    capture = await page.capture()
    assert "Inside the frame" in capture.text
    assert "Visible h1 headings: 0" not in capture.text


async def test_inaccessible_frames_prevent_document_wide_absence_claims(
    chrome_connection: BrowserConnection,
    main_site: str,
) -> None:
    async with scoped(chrome_connection, main_site) as (session, page):
        await page.navigate(main_site)
        await eval_value(session, session.active_session_id, "document.querySelectorAll('h1').forEach(e => e.remove())")
        capture = await page.capture()
        assert capture.inaccessible_frames > 0
        assert "Visible h1 headings: 0" not in capture.text


async def test_empty_frame_documents_emit_one_absence_observation(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await observe_until(page, "Frame button")
    session = page._session
    for frame_session in [session.active_session_id, *session.frame_sessions().values()]:
        await eval_value(session, frame_session, "document.querySelectorAll('h1').forEach(e => e.remove())")
    capture = await page.capture()
    assert capture.inaccessible_frames == 0
    assert capture.text.count("Visible h1 headings: 0") == 1


async def test_control_observations_are_bounded(page: CdpPage, main_site: str) -> None:
    await page.navigate(main_site)
    await eval_value(
        page._session,
        page._session.active_session_id,
        """
        document.body.innerHTML = Array.from({length: 300}, (_, i) =>
          `<label>Field ${i}<input></label><button disabled>Action ${i}</button>`).join('');
        """,
    )
    capture = await page.capture()
    observations = [
        b
        for b in capture.blocks
        if "[empty]" in capture.text[b.start : b.end] or ": disabled" in capture.text[b.start : b.end]
    ]
    assert len(observations) == 64
