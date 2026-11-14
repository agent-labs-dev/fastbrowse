from importlib.metadata import entry_points

import pytest
from fastbrowse.evals import harness
from fastbrowse.models import Status
from parallax.harnesses.base import credential_env_problems

from fastbrowse_evals import backend, hosted_api


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
