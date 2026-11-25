import hashlib
import json
import time
from contextlib import asynccontextmanager
from importlib.metadata import entry_points
from pathlib import Path
from types import SimpleNamespace

import pytest
from fastbrowse.evals import harness
from fastbrowse.models import (
    Artifact,
    ArtifactKind,
    Citation,
    CostBasis,
    CostBreakdown,
    CostComponent,
    CostLine,
    Decider,
    JevPurpose,
    Operation,
    RunResult,
    Status,
    StepOutcome,
    StepResult,
)
from parallax.assets import TrialAssets
from parallax.browser_evidence import BrowserEvidence
from parallax.harnesses.base import credential_env_problems
from parallax.harnesses.browser.cloud import Browser
from pydantic import JsonValue

from fastbrowse_evals import backend, hosted_api


def test_browser_evidence_distinguishes_a_blocked_attempt_from_an_executed_action() -> None:
    step = StepResult(
        index=0,
        operation=Operation.CLICK,
        decided_by=Decider.CODE,
        outcome=StepOutcome.FAILED,
        url="https://shop.example/review",
        target="Place order",
        note="This action needs confirmation",
        duration_ms=0,
    )
    output = RunResult(
        status=Status.NEEDS_CONFIRMATION,
        answer=None,
        data=None,
        evidence=(),
        steps=(step,),
        cost=CostBreakdown(),
        artifacts=(),
    )
    result = backend._run_result(output, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    evidence = BrowserEvidence.model_validate({"answer": None, "steps": result["step_log"]})
    assert evidence.steps[0].action == "click (failed): This action needs confirmation"

    executed = output.model_copy(
        update={"steps": (step.model_copy(update={"outcome": StepOutcome.EXECUTED, "note": None}),)}
    )
    result = backend._run_result(executed, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    evidence = BrowserEvidence.model_validate({"answer": None, "steps": result["step_log"]})
    assert evidence.steps[0].action == "click (executed)"


def test_a_jev_calls_image_count_reaches_parallax_usage() -> None:
    line = CostLine(
        component=CostComponent.JEV,
        basis=CostBasis.METERED,
        decision_purpose=JevPurpose.ACTION,
        input_tokens=4200,
        images=3,
    )
    output = RunResult(
        status=Status.COMPLETE,
        answer="done",
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=(line,)),
        artifacts=(),
    )
    result = backend._run_result(output, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    assert [usage["images"] for usage in result["usage"]] == [3]

    # A campaign also measures builds whose cost lines predate the image count.
    del line.__dict__["images"]
    result = backend._run_result(output, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    assert [usage["images"] for usage in result["usage"]] == [0]


def test_served_models_reach_parallax_usage_and_actual_models() -> None:
    def line(component: CostComponent, model: str | None) -> CostLine:
        return CostLine(component=component, basis=CostBasis.METERED, model=model)

    output = RunResult(
        status=Status.COMPLETE,
        answer="done",
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(
            lines=(
                line(CostComponent.LLM, "openai/gpt-6-luna"),
                line(CostComponent.JEV, "jev-1"),
                line(CostComponent.LLM, "openai/gpt-6-luna"),
                line(CostComponent.LLM, None),
                line(CostComponent.BROWSER, "ignored"),
            )
        ),
        artifacts=(),
    )
    result = backend._run_result(output, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    assert [usage["model"] for usage in result["usage"]] == ["openai/gpt-6-luna", "jev-1", "openai/gpt-6-luna", None]
    assert result["actual_models"] == ["openai/gpt-6-luna", "jev-1"]


@pytest.mark.parametrize("data", [{"name": "widget", "revision": 3}, [], False, 0])
def test_structured_result_reaches_the_judge_instead_of_its_prose(data: JsonValue) -> None:
    output = RunResult(
        status=Status.COMPLETE,
        answer="A prose explanation of the structured result.",
        data=data,
        evidence=(),
        steps=(),
        cost=CostBreakdown(),
        artifacts=(),
    )
    result = backend._run_result(output, None, Browser(cdp_url="ws://browser"), [], latency_ms=0)
    assert isinstance(result["answer"], str)
    assert json.loads(result["answer"]) == data


def test_configured_search_key_reaches_the_product_but_not_an_isolated_adapter(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        backend,
        "environment",
        lambda: {"OPENROUTER_API_KEY": "text", "EXA_API_KEY": "search", "BROWSER_USE_API_KEY": "browser"},
    )
    configured = backend.run_settings()
    assert configured["EXA_API_KEY"] == "search"
    assert "BROWSER_USE_API_KEY" not in configured
    isolated = backend.run_settings({"FASTBROWSE_LLM_PROVIDER": "custom"})
    assert "EXA_API_KEY" not in isolated
    assert "OPENROUTER_API_KEY" not in isolated


@pytest.mark.parametrize("status", [Status.NEEDS_LOGIN, Status.NEEDS_CONFIRMATION, Status.STUCK, Status.BLOCKED])
def test_product_terminal_statuses_are_gradeable_and_preserved(status: Status) -> None:
    backend_harness = backend.FastbrowseHarness(observer="plain-cdp")
    backend_harness._product_status = status
    hosted_harness = hosted_api.FastbrowseHostedHarness(observer="plain-cdp")
    hosted_harness._product_status = status

    assert backend._turn_status(status) == "done"
    assert hosted_api._turn_status(status) == "done"
    assert backend_harness.trial_details()["status"] == status.value
    assert hosted_harness.trial_details()["status"] == status.value


@pytest.mark.parametrize(
    ("state", "status"),
    [("succeeded", Status.COMPLETE), ("expired", Status.UNAVAILABLE), ("canceled", Status.CANCELLED)]
    + [(status.value, status) for status in Status],
)
def test_hosted_product_status_mapping_is_explicit(state: str, status: Status) -> None:
    assert hosted_api._product_status(state) is status


def test_hosted_unknown_product_status_is_not_silently_an_error() -> None:
    with pytest.raises(ValueError, match="not a valid Status"):
        hosted_api._product_status("future-status")


@pytest.mark.parametrize("name", ["fastbrowse", "fastbrowse-pool", "fastbrowse-hosted"])
def test_every_fastbrowse_harness_declares_credentials_parallax_will_forward(name: str) -> None:
    """Parallax refuses, at each trial's start, a harness naming a key its meter serves or one of its own."""
    [point] = entry_points(group="parallax.harnesses", name=name)
    assert credential_env_problems(point.load().info) == []


def test_the_arm_reads_its_openrouter_key_under_its_own_name(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        harness, "environment", lambda: {"FASTBROWSE_OPENROUTER_API_KEY": "arm", "OPENROUTER_API_KEY": "meter"}
    )
    assert backend.run_settings()["OPENROUTER_API_KEY"] == "arm"


def test_both_arms_cite_in_the_shape_browser_evidence_accepts() -> None:
    """A field Parallax's evidence does not hold fails the whole run, not one trial."""
    cited = Citation(id=1, text="A Light in the Attic", url="https://books.example/1", quote="£51.77", deep_link="x")
    run = json.loads((Path(__file__).parent / "fixtures" / "fastbrowse_hosted_run.json").read_text())
    for citations in (backend.evidence_citations([cited]), hosted_api.evidence_citations(run)):
        BrowserEvidence.model_validate({"final_page": None, "answer": None, "citations": citations})


def test_a_page_read_over_http_is_the_evidence_its_read_step_points_at() -> None:
    """The browser never left its blank tab: the frames page the run read over HTTP is the only record of it."""
    url = "https://practice.expandtesting.com/frames"
    text = "Frames\nSend updates to my inbox"
    pages, seen_at = backend.read_pages(
        [
            {"event": "route", "providers": {}},
            {
                "event": "read",
                "step": 0,
                "url": url,
                "title": "Frames",
                "sha256": "a",
                "chars": 30,
                "capture": {"url": url, "title": "Frames", "sha256": "a", "text": text, "blocks": []},
            },
            {"event": "page", "step": 1, "url": url, "title": "Frames", "status": 200, "text": text},
        ]
    )
    evidence = BrowserEvidence.model_validate(
        {
            "final_page": None,
            "answer": "Send updates to my inbox",
            "steps": [{"action": "read", "url": url, "page": seen_at[0]}],
            "pages": pages,
            "citations": [{"url": url, "quote": "Send updates to my inbox"}],
        }
    )
    assert [(page.url, page.text) for page in evidence.pages] == [(url, text)]
    assert evidence.steps[0].page == 0 and seen_at[1] == 0


def test_a_page_whose_field_values_changed_is_a_new_page() -> None:
    """Three fills on one checkout form left its text alone, and the judge was shown only the first. A read of the
    filled form, which carries no controls, is the page it was read from."""
    url, text = "https://www.saucedemo.com/checkout-step-one.html", "Checkout: Your Information"
    fields = [("Ada", "", ""), ("Ada", "Lovelace", ""), ("Ada", "Lovelace", "EC1A 1BB")]
    events: list[object] = [
        {
            "event": "page",
            "step": step,
            "url": url,
            "title": "Swag Labs",
            "text": text,
            "controls": [
                {"label": label, "value": value} for label, value in zip(("First", "Last", "Zip"), values, strict=True)
            ],
        }
        for step, values in enumerate(fields)
    ]
    events.append({"event": "read", "step": 3, "url": url, "title": "Swag Labs", "capture": {"text": text}})
    pages, seen_at = backend.read_pages(events)
    assert [page["controls"][2]["value"] for page in pages] == ["", "", "EC1A 1BB"]
    assert seen_at == {0: 0, 1: 1, 2: 2, 3: 2}


def test_campaign_metadata_preserves_model_requests_outside_judge_evidence() -> None:
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    event = {"event": "llm_request", "messages": [{"role": "user", "content": "Captured notes"}]}
    harness._events.append(event)
    metadata = json.loads(json.dumps(harness.trial_metadata))
    assert metadata["events"] == [event]
    assert "events" not in metadata["browser"]


def test_campaign_metadata_keeps_the_typed_capture_and_evidence_a_read_traced() -> None:
    """The exported trajectory is what a composition replay rebuilds its notes from (#1273): the capture's blocks
    with their source ids, kind and whole-record flag, its sha, and each fact's evidence binding, all unaltered."""
    url = "https://example.test/files"
    text = "Download files\nReleased: 2026-01-01"
    block = {
        "source_id": "s1",
        "kind": "paragraph",
        "start": 15,
        "end": 35,
        "heading_path": ["Download files"],
        "whole_record": True,
    }
    read = {
        "event": "read",
        "step": 0,
        "url": url,
        "sha256": "abc",
        "capture": {"url": url, "title": "Files", "sha256": "abc", "text": text, "blocks": [block]},
        "facts": [{"text": "Released: 2026-01-01", "evidence": {"source_id": "s1", "capture_sha256": "abc"}}],
    }
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._events.append(read)
    assert json.loads(json.dumps(harness.trial_metadata))["events"] == [read]


def test_campaign_traces_scrub_bound_task_secrets() -> None:
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._secret_values = {"PASSWORD": "fixture-private-value"}
    harness._events.append({"event": "llm_request", "text": "Use fixture-private-value"})
    metadata = harness.trial_metadata
    assert "fixture-private-value" not in json.dumps(metadata)
    assert metadata["events"] == [{"event": "llm_request", "text": "Use <secret>PASSWORD</secret>"}]


def test_a_cancelled_run_reports_the_pages_and_steps_it_reached() -> None:
    """2026-11-17 paper smoke: a run stuck in a wait (#1213) was cancelled at Parallax's 1800 s turn limit and
    reported no page or step, so the trial graded invalid and was retried (#1215). Its recorded trace events."""
    events = json.loads((Path(__file__).parent / "fixtures" / "cancelled_wait_events.json").read_text())
    evidence = backend.cancelled_evidence(events)
    url = "https://practice.expandtesting.com/dynamic-loading/2"
    assert [step["action"] for step in evidence["step_log"]] == ["click", "wait (cancelled)"]
    assert evidence["step_log"][-1]["url"] == url
    pages = evidence["pages"]
    assert isinstance(pages, list) and pages[evidence["step_log"][-1]["page"]]["url"] == url
    BrowserEvidence.model_validate(
        {"final_page": None, "answer": None, "steps": evidence["step_log"], "pages": pages, "citations": []}
    )


async def test_a_cloud_browser_lives_only_as_long_as_its_turn(monkeypatch: pytest.MonkeyPatch) -> None:
    """A browser whose stop is missed (the process was killed) stops itself when its turn ends, not after Browser
    Use's 240 minute default (#1264)."""
    asked: list[int] = []

    @asynccontextmanager
    async def cloud_browser(http: object, *, screen: tuple[int, int], minutes: int):
        asked.append(minutes)
        yield Browser(cdp_url="ws://browser")

    monkeypatch.setattr(backend, "cloud_browser", cloud_browser)
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._turn_ends_at = time.monotonic() + 600
    async with harness.browser(None):  # ty: ignore[invalid-argument-type]
        pass
    assert asked == [10]


def _kept(directory: Path, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact:
    """An artifact as fastbrowse's directory sink keeps it: the bytes on disk and a `file://` reference."""
    path = directory / kind.value / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    digest = hashlib.sha256(content).hexdigest()
    return Artifact(
        kind=kind, name=name, mime_type=mime_type, size_bytes=len(content), sha256=digest, uri=path.as_uri()
    )


def test_the_screenshot_an_image_reading_read_is_retained_under_the_reference_its_trace_cites(tmp_path: Path) -> None:
    """Campaign 43745284709: an image reading's input was a redacted `data:image/<image>` in its receipt, and its
    PNG was kept nowhere, so the wrong reading could not be replayed (#1150)."""
    png = b"\x89PNG\r\n\x1a\nviewport"
    digest = hashlib.sha256(png).hexdigest()
    screenshot = _kept(tmp_path / "run", ArtifactKind.SCREENSHOT, f"screenshot-{digest}.png", "image/png", png)
    capture = _kept(tmp_path / "run", ArtifactKind.CAPTURE, "prices.txt", "text/plain; charset=utf-8", b"USD 986")
    assets = TrialAssets(tmp_path / "trial")
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._trial = SimpleNamespace(assets=assets)  # ty: ignore[invalid-assignment]

    harness.retain([screenshot, capture])

    assert assets.read(f"assets/{digest}") == (png, "image/png")
    assert assets.read(f"assets/{capture.sha256}") == (b"USD 986", "text/plain")
    assert assets.validate() == ()
    assert json.loads(json.dumps(harness.trial_metadata))["assets"] == [
        {"kind": "screenshot", "name": f"screenshot-{digest}.png", "asset": f"assets/{digest}"},
        {"kind": "capture", "name": "prices.txt", "asset": f"assets/{capture.sha256}"},
    ]


def test_an_empty_download_and_one_content_under_two_media_types_do_not_abort_retention(tmp_path: Path) -> None:
    """fastbrowse keeps an empty download and the same bytes under two media types; a trial asset takes neither
    as is, and the registration error would discard the completed result."""
    run = tmp_path / "run"
    empty = _kept(run, ArtifactKind.DOWNLOAD, "empty.csv", "text/csv", b"")
    csv = b"item,price\nbook,12\n"
    first = _kept(run, ArtifactKind.DOWNLOAD, "report-a.csv", "text/csv", csv)
    second = _kept(run, ArtifactKind.DOWNLOAD, "report-b.csv", "application/octet-stream", csv)
    assets = TrialAssets(tmp_path / "trial")
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._trial = SimpleNamespace(assets=assets)  # ty: ignore[invalid-assignment]

    harness.retain([empty, first, second])

    assert assets.validate() == ()
    assert assets.read(f"assets/{first.sha256}") == (csv, "text/csv")
    assert harness.trial_metadata["assets"] == [
        {"kind": "download", "name": "empty.csv", "asset": None},
        {"kind": "download", "name": "report-a.csv", "asset": f"assets/{first.sha256}"},
        {"kind": "download", "name": "report-b.csv", "asset": f"assets/{first.sha256}"},
    ]


def test_a_task_that_binds_secrets_retains_no_run_artifact(tmp_path: Path) -> None:
    """A bound login can show in a screenshot's pixels, which the trace's text scrubbing cannot reach."""
    screenshot = _kept(tmp_path / "run", ArtifactKind.SCREENSHOT, "login.png", "image/png", b"\x89PNG\r\n\x1a\nlogin")
    assets = TrialAssets(tmp_path / "trial")
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    harness._trial = SimpleNamespace(assets=assets)  # ty: ignore[invalid-assignment]
    harness._secret_values = {"PASSWORD": "fixture-private-value"}

    harness.retain([screenshot])

    assert not assets.root.exists()
    assert "assets" not in harness.trial_metadata
    assert harness.trial_notes == ["run artifacts not retained: the task binds secrets"]
