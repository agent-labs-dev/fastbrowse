"""A styled upload label must retain the file input it submits."""

from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.models import Attachment, Operation, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import eval_value


async def test_visible_label_uploads_to_hidden_file_input(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<input id="file" type="file" style="display:none">' +
            '<label for="file">Upload or Drag Image</label>';
    """,
    )
    observation = await page.observe()
    control = next(c for c in observation.controls if c.label == "Upload or Drag Image")
    assert control.operations == frozenset({Operation.UPLOAD})
    result = await page.act(
        Action(
            operation=Operation.UPLOAD,
            target_id=control.id,
            files=(Attachment(name="image.png", mime_type="image/png", content=b"provided bytes"),),
        ),
        observation,
    )
    assert result.outcome is StepOutcome.EXECUTED
    result = await browser_session.client.send.Runtime.evaluate(
        params={
            "expression": "document.getElementById('file').files[0].text()",
            "awaitPromise": True,
            "returnByValue": True,
        },
        session_id=browser_session.active_session_id,
    )
    assert result["result"]["value"] == "provided bytes"
