"""The `on_frame` cancellation contract is documented where a caller will read it.

A handler runs on the caller's event loop, so one that catches `CancelledError` without re-raising keeps that
loop alive, and `asyncio.run` then waits for it at shutdown. That contract is stated in the API docstring, the
handler type and the user docs; this test keeps all three saying it, since the contract is the fix for #350.
"""

import inspect
from pathlib import Path

from fastbrowse.run import run_task

ROOT = Path(__file__).resolve().parents[1]
_CONTRACT = "must honour cancellation"


def test_run_task_docstring_states_the_on_frame_cancellation_contract() -> None:
    doc = inspect.getdoc(run_task)
    assert doc is not None and _CONTRACT in doc


def test_frame_handler_type_documents_the_cancellation_contract() -> None:
    source = (ROOT / "src" / "fastbrowse" / "models.py").read_text(encoding="utf-8")
    frame_handler = source.split("type FrameHandler", 1)[1].split("type UntilCheck", 1)[0]
    assert _CONTRACT in frame_handler


def test_readme_states_the_on_frame_cancellation_contract() -> None:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert _CONTRACT in readme


def test_design_doc_states_the_on_frame_cancellation_contract() -> None:
    design = (ROOT / "docs" / "design.md").read_text(encoding="utf-8")
    assert _CONTRACT in design
