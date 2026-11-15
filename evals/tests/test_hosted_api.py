import asyncio
import json
import os
from pathlib import Path

import httpx
import pytest
from parallax.models import Area, Capability, TaskRecord
from parallax.task import task_from_record

from fastbrowse_evals import hosted_api

# A real `GET /v1/runs/{id}` body for a finished live run (recorded 2026-11-05 with a live key).
RUN = Path(__file__).parent / "fixtures" / "fastbrowse_hosted_run.json"
TASK = task_from_record(
    "fastbrowse",
    TaskRecord(
        id="simulator",
        version=1,
        area=Area.BROWSER,
        summary="Report the page title.",
        prompt="Report the page title.",
        grading="browser-evidence-v1",
        requires=frozenset({Capability.BROWSER}),
        data={"start_url": "https://example.com/"},
    ),
)


def test_a_recorded_live_run_reports_the_charge_the_worker_time_and_the_citations() -> None:
    run = json.loads(RUN.read_text())
    report = hosted_api.report_fields(run, wall=108.4)
    outcome = hosted_api.outcome_of(run)

    assert report["status"] == "complete"
    assert report["dollars"] == pytest.approx(0.5742)
    assert report["seconds"] == pytest.approx(105.64)
    assert report["timing_basis"] is None
    assert report["input_tokens"] == sum(c["input_tokens"] for c in run["usage"]["components"].values())
    assert outcome["final_url"] == run["final_url"]
    assert ("https://books.toscrape.com/catalogue/a-light-in-the-attic_1000/index.html", "A Light in the Attic") in {
        (c["url"], c["quote"]) for c in outcome["citations"]
    }


@pytest.mark.hosted_simulator
async def test_the_hosted_api_simulator_answers_in_the_shapes_the_arm_reads() -> None:
    """Against the live API with a `fbh_test_` key: no browser and no charge, but the real submit, poll and run
    body, so a change to the public API's shapes, or a request field it refuses, fails here before it costs a
    campaign."""
    assert os.environ.get("FASTBROWSE_HOSTED_API_KEY", "").startswith("fbh_test_"), "needs a fbh_test_ key"
    async with httpx.AsyncClient() as http:
        run, wall = await hosted_api.submit_and_wait(http, TASK, TASK.turns[0].text)

    report = hosted_api.report_fields(run, wall)
    assert run["state"] == "succeeded"
    assert report["status"] == "complete"
    assert report["dollars"] == 0
    assert report["provenance"]["simulator"] is True
    assert hosted_api.outcome_of(run)["citations"]


async def test_a_run_accepted_as_the_attempt_is_cancelled_is_cancelled_too(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FASTBROWSE_HOSTED_API_KEY", "test-key")
    reply, cancelled = asyncio.Event(), []

    async def handle(request: httpx.Request) -> httpx.Response:
        if request.method == "POST" and request.url.path == "/v1/runs":
            await reply.wait()
            return httpx.Response(201, json={"id": "run-1", "state": "running"})
        if request.url.path.endswith("/cancel"):
            cancelled.append(request.url.path)
            return httpx.Response(200, json={"state": "canceled"})
        return httpx.Response(200, json={"id": "run-1", "state": "succeeded", "status": "complete"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handle)) as http:
        running = asyncio.create_task(hosted_api.submit_and_wait(http, TASK, TASK.turns[0].text))
        await asyncio.sleep(0.01)
        running.cancel()
        reply.set()
        with pytest.raises(asyncio.CancelledError):
            await running
    assert cancelled == ["/v1/runs/run-1/cancel"]
