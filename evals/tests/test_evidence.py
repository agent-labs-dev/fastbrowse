"""The judge's view of a run under the evidence policy: one form for every arm, nothing cut."""

import asyncio

from parallax.judging import Decision, Judgment

from fastbrowse_evals import judging
from fastbrowse_evals.evidence import parts, run_evidence
from fastbrowse_evals.live import JUDGE_POLICY, Ending, no_evidence, shown_to


def test_a_fetched_run_shows_the_pages_it_read():
    report = {"events": [{"event": "read", "step": 0, "url": "https://a.test", "text": "the whole page"}]}
    evidence = run_evidence(report, [["https://a.test", "whole"]])
    assert evidence["pages"] == [{"id": "page-1", "url": "https://a.test", "text": "the whole page"}]
    assert evidence["citations"] == [{"url": "https://a.test", "quote": "whole"}]
    assert evidence["evidence_recorded"] == {"actions": False, "pages": True, "citations": True}


def test_every_arm_gets_the_same_timeline_and_pages():
    from_events = run_evidence(
        {
            "events": [
                {"event": "page", "step": 0, "url": "u", "text": "form"},
                {"event": "typed", "step": 0, "value": "Ada"},
                {"event": "http", "step": 0, "request": "a model's prompt"},
            ],
            "step_log": [{"index": 0, "operation": "fill", "target": "Name", "facts": [], "page_changed": True}],
        },
        None,
    )
    from_steps = run_evidence(
        {
            "step_log": [
                {"operation": "browser_type_text", "url": "u", "target": {"text": "Ada"}, "observation": "form"},
                {"operation": "browser_click", "url": "u", "observation_same_as_step": 0},
            ]
        },
        None,
    )
    assert from_events["timeline"] == [
        {"index": 0, "operation": "fill", "target": "Name", "saw": ["page-1"], "typed": "Ada"}
    ]
    assert [s["saw"] for s in from_steps["timeline"]] == [["page-1"], ["page-1"]]
    assert from_steps["timeline"][0]["operation"] == "fill"
    assert from_events["pages"][0]["text"] == from_steps["pages"][0]["text"] == "form"


def test_the_policy_state_keeps_no_check_source_or_arm_detail():
    state = {
        "task": "t",
        "expected_ending": "complete",
        "requirements": "def check(): ...",
        "quotes": [],
        "report": {"status": "done", "model": "secret-arm", "dollars": 1.0, "seconds": 3.0, "events": []},
    }
    shown = shown_to(JUDGE_POLICY, state, Ending.DONE)
    assert "requirements" not in shown
    assert set(shown["report"]) == {"ending", "error", "observe_error", "failure_class", "steps"}
    assert shown_to(JUDGE_POLICY, shown, Ending.DONE) == shown
    assert no_evidence(shown)


def test_a_state_over_the_window_is_divided_without_losing_text():
    long = "é" * 5000
    state = {"task": "t", "pages": [{"id": "page-1", "text": long}, {"id": "page-2", "text": "short"}]}
    divided = parts(state, limit=4096)
    assert len(divided) > 1
    assert all(len(str(part).encode()) <= 4096 * 2 for part in divided)
    assert "".join(p["text"] for part in divided for p in part["pages"] if p["id"] == "page-1") == long
    assert any(p["id"] == "page-2" for part in divided for p in part["pages"])
    assert parts(state) == [state]


def test_part_verdicts_combine_any_false_then_any_true(monkeypatch):
    verdicts = iter([None, False, True])

    async def judged(part, *, policy, questions, **_):
        decision = Decision(passed=next(verdicts), source="llm", reason="r")
        return Judgment(policy=policy, state_sha256="x", questions={}, decisions={"q": decision}), "id"

    monkeypatch.setattr(judging, "judge_browser", judged)
    result, _ = asyncio.run(judging.judge_browser_parts({"whole": 1}, [{}, {}, {}], policy="p", questions={"q": "?"}))
    assert result.decisions["q"].passed is False
    assert result.state_sha256 != "x"
