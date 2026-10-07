import importlib.util
import json
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.clients.environment import Settings
from fastbrowse.evals.tasks import TASKS

_spec = importlib.util.spec_from_file_location(
    "eval_stability", Path(__file__).parents[1] / "scripts" / "eval_stability.py"
)
assert _spec is not None and _spec.loader is not None
eval_stability = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(eval_stability)


@pytest.fixture(autouse=True)
def no_ambient_keys(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in (
        "TYPESAFE_API_KEY",
        "AI_GATEWAY_API_KEY",
        "OPENROUTER_API_KEY",
        "FASTBROWSE_JEV_SOURCE",
        "FASTBROWSE_JEV_BASE_URL",
        "FASTBROWSE_JEV_MODEL",
    ):
        monkeypatch.delenv(name, raising=False)


def settings(**values: Any) -> Settings:
    return Settings(_env_file=None, **values)


def written(path: Path, rows: list[dict[str, object]]) -> Path:
    path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
    return path


def task_rows(suite: str, outcome: dict[str, bool] | None = None) -> list[dict[str, object]]:
    outcome = outcome or {}
    return [{"suite": suite, "task": task.id, "passed": outcome.get(task.id, True)} for task in TASKS]


def test_the_gateway_key_covers_both_models() -> None:
    gaps = eval_stability.provider_gaps(settings(AI_GATEWAY_API_KEY="g"))
    assert gaps == []


def test_openrouter_covers_both_models() -> None:
    assert eval_stability.provider_gaps(settings(OPENROUTER_API_KEY="o")) == []


def test_no_provider_names_every_missing_setting() -> None:
    assert eval_stability.provider_gaps(settings()) == [
        "OPENROUTER_API_KEY or AI_GATEWAY_API_KEY: the LLM needs a configured provider",
        "AI_GATEWAY_API_KEY: Jev starts on the gateway route",
    ]


def test_check_providers_names_the_missing_llm_key(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(eval_stability, "load_settings", lambda: settings(TYPESAFE_API_KEY="t"))
    assert eval_stability.main(["--check-providers"]) == 1
    out = capsys.readouterr().out
    assert "OPENROUTER_API_KEY or AI_GATEWAY_API_KEY: the LLM needs a configured provider" in out


def test_check_providers_passes_when_both_routes_are_keyed(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        eval_stability, "load_settings", lambda: settings(OPENROUTER_API_KEY="o", AI_GATEWAY_API_KEY="g")
    )
    assert eval_stability.main(["--check-providers"]) == 0
    assert "model providers ready" in capsys.readouterr().out


def test_a_missing_row_file_says_the_runner_recorded_nothing(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    assert eval_stability.main([str(tmp_path / "nightly.jsonl")]) == 1
    assert "no eval rows" in capsys.readouterr().out


def test_an_empty_row_file_says_the_runner_executed_no_task(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    path = written(tmp_path / "nightly.jsonl", [])
    assert eval_stability.main([str(path)]) == 1
    assert "no eval rows" in capsys.readouterr().out


def test_a_complete_passing_run_clears_the_stability_gate(tmp_path: Path) -> None:
    path = written(tmp_path / "nightly.jsonl", task_rows("local"))
    assert eval_stability.main([str(path), "--suite", "local", "--repeat", "1"]) == 0


def test_a_flaky_task_fails_the_stability_gate(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    flaky = TASKS[0].id
    rows: list[dict[str, object]] = []
    for task in TASKS:
        rows.append({"suite": "local", "task": task.id, "passed": task.id != flaky})
        rows.append({"suite": "local", "task": task.id, "passed": True})
    path = written(tmp_path / "nightly.jsonl", rows)
    assert eval_stability.main([str(path), "--suite", "local", "--repeat", "2"]) == 1
    assert "FLAKY" in capsys.readouterr().out


def test_a_run_missing_a_task_fails_the_stability_gate(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    rows = task_rows("local")[:-1]
    path = written(tmp_path / "nightly.jsonl", rows)
    assert eval_stability.main([str(path), "--suite", "local", "--repeat", "1"]) == 1
    assert "incomplete run" in capsys.readouterr().out
