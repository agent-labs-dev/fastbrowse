import json
from importlib.metadata import entry_points
from pathlib import Path

import pytest
from fastbrowse.evals import harness
from fastbrowse.models import (
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
            {"event": "read", "step": 0, "url": url, "title": "Frames", "sha256": "a", "text": text, "chars": 30},
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


def test_campaign_metadata_preserves_model_requests_outside_judge_evidence() -> None:
    harness = backend.FastbrowseHarness(observer="plain-cdp")
    event = {"event": "llm_request", "messages": [{"role": "user", "content": "Captured notes"}]}
    harness._events.append(event)
    metadata = json.loads(json.dumps(harness.trial_metadata))
    assert metadata["events"] == [event]
    assert "events" not in metadata["browser"]


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
