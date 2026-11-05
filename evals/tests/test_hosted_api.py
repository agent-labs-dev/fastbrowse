import json
import os
import types
from pathlib import Path

import httpx
import pytest

from fastbrowse_evals import hosted_api

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
