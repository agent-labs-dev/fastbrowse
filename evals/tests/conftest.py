"""Shared fixtures for the browser benchmark tests."""

import pytest

from fastbrowse_evals import judging, live, official


@pytest.fixture(autouse=True)
def judges_configured(request, monkeypatch):
    """Campaigns refuse to start without their judges. Offline tests stub the judge itself, so the start check
    passes; a test of the check opts out with `@pytest.mark.real_judge_check`."""
    if "real_judge_check" not in request.keywords:
        for module in (live, judging, official):
            monkeypatch.setattr(module, "require_judges", lambda **_: None)


AVAILABLE_CHECK = {
    "judgment": {
        "policy": official.INTERRUPTION_POLICY,
        "state_sha256": "0" * 64,
        "questions": official.INTERRUPTION_QUESTIONS,
        "decisions": {"evidence_available": {"passed": True, "source": "llm", "reason": "observed"}},
    },
    "policy_sha256": "f" * 64,
    "judge_cost_usd": 0.0,
    "known_judge_cost_usd": 0.0,
    "unpriced_judge_attempts": 0,
}


@pytest.fixture(autouse=True)
def no_site_interruption(request, monkeypatch):
    """Official runs ask the harness judge whether the site interrupted them; offline, it never did, unless a test
    replaces the check itself."""
    if "real_interruption_check" not in request.keywords:

        async def available(*_, **__):
            return AVAILABLE_CHECK

        monkeypatch.setattr(official, "interruption_check", available)
