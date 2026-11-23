import json
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast

from parallax.models import ENVIRONMENT_INTERRUPTION, TurnInput, TurnStatus
from parallax.runner import desktop_commands
from parallax.task import load_task

from fastbrowse_evals.arms import invocations
from fastbrowse_evals.desktop.harness import FastbrowseDesktopHarness, claude_code_deskpilot, turn_output

TASK = load_task(Path(__file__).parents[1] / "tasks" / "files-desktop-report-cross-app")

LLM = {
    "component": "llm",
    "dollars": 0.02,
    "input_tokens": 900,
    "output_tokens": 40,
    "physical_requests": 3,
    "model": "m",
}
STEP = {"operation": "click", "outcome": "executed", "target": "Save", "note": None, "duration_ms": 250}


def test_the_arm_starts_each_app_the_task_names_and_nothing_else() -> None:
    harness = FastbrowseDesktopHarness()
    harness.bind_task(TASK)
    desktop = TASK.meta.desktop
    assert desktop is not None
    harness._trial = cast(Any, SimpleNamespace(home=Path("/home/t"), desktop_commands=desktop_commands(TASK)))
    argv = harness.argv(
        TurnInput(index=0, text="do it", new_session=True), continuing=False, session_id=None, prompt_file=Path("p")
    )
    launches = [argv[i + 1] for i, word in enumerate(argv) if word == "--desktop"]
    assert launches == [" ".join(app.command) for app in desktop.apps]
    assert argv[-2:] == ["--", "do it"]
    assert "--authorize" not in argv


def test_the_baseline_is_told_the_same_apps_as_the_task() -> None:
    desktop = TASK.meta.desktop
    assert desktop is not None
    prompt = claude_code_deskpilot().spec.argv_first[-1]
    for app in desktop.apps:
        assert json.dumps(list(app.command)) in prompt


def test_a_run_that_printed_nothing_is_a_harness_failure() -> None:
    out = turn_output({}, "boom")
    assert out.status is TurnStatus.HARNESS_ERROR
    assert "boom" in (out.error or "")


def test_any_status_the_product_reaches_is_judged_from_the_workspace() -> None:
    result = {"status": "needs_confirmation", "answer": "a", "steps": [STEP], "cost": {"lines": [LLM]}}
    out = turn_output(result, "")
    assert out.status is TurnStatus.COMPLETED
    assert out.usage[0].cost_usd == 0.02
    assert out.steps[0].name == "click"
    assert out.steps[0].ok


def test_an_outage_is_the_environments_and_an_error_is_the_arms() -> None:
    unavailable = turn_output({"status": "unavailable", "error": "502"}, "")
    assert unavailable.status is TurnStatus.HARNESS_ERROR
    assert (unavailable.error or "").startswith(ENVIRONMENT_INTERRUPTION)
    failed = turn_output({"status": "error", "error": "no app"}, "")
    assert failed.error == "no app"
    assert not (failed.error or "").startswith(ENVIRONMENT_INTERRUPTION)


def test_both_arms_are_bound() -> None:
    fastbrowse, claude = invocations(["fastbrowse-desktop", "claude-code-deskpilot"])
    assert (fastbrowse.harness, fastbrowse.model) == ("fastbrowse-desktop", None)
    assert claude.harness == "claude-code-deskpilot"
    assert claude.model
