import asyncio
import json
import os
import types
from pathlib import Path

import httpx
import pytest

from fastbrowse_evals import attempt_clock, hosted_api

# A real `GET /v1/runs/{id}` body for a finished live run (recorded 2026-11-05 with a live key).
RUN = Path(__file__).parent / "fixtures" / "fastbrowse_hosted_run.json"


def test_a_recorded_live_run_reports_the_charge_the_worker_time_and_the_quotes() -> None:
    run = json.loads(RUN.read_text())
    report = hosted_api.report_fields(run, wall=108.4)
    outcome = hosted_api.outcome_of(run)

    assert report["status"] == "complete"
    assert report["dollars"] == pytest.approx(0.5742)
    assert report["seconds"] == pytest.approx(105.64)
    assert report["timing_basis"] is None
    assert report["input_tokens"] == sum(c["input_tokens"] for c in run["usage"]["components"].values())
    assert outcome.final_url == run["final_url"]
    assert ("https://books.toscrape.com/catalogue/a-light-in-the-attic_1000/index.html", "A Light in the Attic") in (
        outcome.quotes
    )


@pytest.mark.hosted_simulator
async def test_the_hosted_api_simulator_answers_in_the_shapes_the_arm_reads() -> None:
    """Against the live API with a `fbh_test_` key: no browser and no charge, but the real submit, poll and run
    body, so a change to the public API's shapes fails here before it costs a campaign."""
    assert os.environ.get("FASTBROWSE_HOSTED_API_KEY", "").startswith("fbh_test_"), "needs a fbh_test_ key"
    task = types.SimpleNamespace(id="simulator", task="Report the page title.", start="https://example.com/")
    async with httpx.AsyncClient() as http:
        run, wall = await hosted_api.submit_and_wait(http, task)

    report = hosted_api.report_fields(run, wall)
    assert run["state"] == "succeeded"
    assert report["status"] == "complete"
    assert report["dollars"] == 0
    assert report["provenance"]["simulator"] is True
    assert hosted_api.outcome_of(run).quotes


def _service(*, reply: asyncio.Event | None = None, cancelled: list[str], poll_seconds: float = 0.0):
    async def handle(request: httpx.Request) -> httpx.Response:
        if request.method == "POST" and request.url.path == "/v1/runs":
            if reply is not None:
                await reply.wait()
            return httpx.Response(201, json={"id": "run-1", "state": "running"})
        if request.url.path.endswith("/cancel"):
            cancelled.append(request.url.path)
            return httpx.Response(200, json={"state": "canceled"})
        await asyncio.sleep(poll_seconds)
        return httpx.Response(200, json={"id": "run-1", "state": "succeeded", "status": "complete"})

    return handle


TASK = types.SimpleNamespace(id="t", task="Read the title", start="https://example.com")


async def test_a_run_accepted_as_the_attempt_is_cancelled_is_cancelled_too(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FASTBROWSE_HOSTED_API_KEY", "test-key")
    reply, cancelled = asyncio.Event(), []
    async with httpx.AsyncClient(transport=httpx.MockTransport(_service(reply=reply, cancelled=cancelled))) as http:
        running = asyncio.create_task(hosted_api.submit_and_wait(http, TASK))
        await asyncio.sleep(0.01)
        running.cancel()
        reply.set()
        with pytest.raises(asyncio.CancelledError):
            await running
    assert cancelled == ["/v1/runs/run-1/cancel"]


async def test_a_hosted_run_outlives_the_session_opening_deadline(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FASTBROWSE_HOSTED_API_KEY", "test-key")
    monkeypatch.setattr(attempt_clock, "SESSION_OPEN_SECONDS", 0.02)
    monkeypatch.setattr(hosted_api, "POLL_SECONDS", 0)
    cancelled: list[str] = []
    transport = httpx.MockTransport(_service(cancelled=cancelled, poll_seconds=0.06))
    async with httpx.AsyncClient(transport=transport) as http, attempt_clock.AttemptClock(bounded=True) as clock:
        run, _ = await hosted_api.submit_and_wait(http, TASK)
    assert run["state"] == "succeeded"
    assert clock.phase == "close"
    assert not cancelled
