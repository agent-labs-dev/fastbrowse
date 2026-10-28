from pathlib import Path

import pytest

from fastbrowse.evals.harness import TaskSecrets, environment, origin_of, providers, transient_seconds


def test_the_environment_reads_the_dotenv_beside_it_under_real_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dotenv = tmp_path / ".env"
    dotenv.write_text(
        "# a comment\n"
        "TYPESAFE_API_KEY=direct  # the recommended route\n"
        "export OPENROUTER_API_KEY='router'\n"
        "AI_GATEWAY_API_KEY=\n"
        "FASTBROWSE_JEV_SOURCE=openrouter\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FASTBROWSE_JEV_SOURCE", "typesafe")
    monkeypatch.delenv("TYPESAFE_API_KEY", raising=False)
    monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
    monkeypatch.delenv("AI_GATEWAY_API_KEY", raising=False)

    read = environment(dotenv)

    assert (read["TYPESAFE_API_KEY"], read["OPENROUTER_API_KEY"]) == ("direct", "router")
    assert "AI_GATEWAY_API_KEY" not in read
    assert read["FASTBROWSE_JEV_SOURCE"] == "typesafe"


def test_transient_time_counts_overlapping_calls_once_and_only_within_the_run() -> None:
    def span(began: float, ended: float) -> dict[str, object]:
        return {"event": "request_transient", "call": "jev", "began": began, "ended": ended}

    # Parallel batches failing over 2..5 and 4..6 lost 4s, not 5; a span before the run and other events add none.
    events = [span(4.0, 6.0), span(2.0, 5.0), span(-3.0, -1.0), {"event": "request_retry"}, "a message"]
    assert transient_seconds(events, 0.0, 10.0) == pytest.approx(4.0)
    assert transient_seconds(events, 0.0, 3.0) == pytest.approx(1.0)


def test_the_route_is_read_from_the_runs_route_event() -> None:
    assert providers([{"event": "request_retry"}, {"event": "route", "providers": "Jev x; LLM y"}]) == "Jev x; LLM y"
    assert providers([{"event": "request_retry"}]) is None


@pytest.mark.parametrize(
    ("url", "origin"),
    [
        ("http://127.0.0.1:8123/sign-in?next=/", "http://127.0.0.1:8123"),
        ("https://Shop.Example.com:443/", "https://shop.example.com"),
        ("http://user@host.test:80/", "http://host.test"),
    ],
)
def test_an_origin_drops_the_path_credentials_and_an_implied_port(url: str, origin: str) -> None:
    assert origin_of(url) == origin


async def test_task_secrets_are_given_only_on_the_fixture_origin() -> None:
    secrets = TaskSecrets({"password": "hunter2"}, "http://127.0.0.1:8123/sign-in")
    assert [(ref.name, ref.origins) for ref in secrets.available()] == [("password", ("http://127.0.0.1:8123",))]
    assert await secrets.resolve("password", "http://127.0.0.1:8123") == "hunter2"
    assert await secrets.resolve("password", "http://127.0.0.1:8124") is None
    assert await secrets.resolve("other", "http://127.0.0.1:8123") is None
