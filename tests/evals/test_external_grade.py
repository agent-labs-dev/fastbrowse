"""The external graders: pinned answer predicates, and a trusted local command behind a JSON stdio bridge."""

import asyncio
import os
import sys
import time
from pathlib import Path

import pytest

from fastbrowse.evals import external_grade as grade_module
from fastbrowse.evals.corpus import TaskRef
from fastbrowse.evals.external_grade import (
    GraderError,
    UnsupportedPredicate,
    grade_answer_predicate,
    local_command_grader,
    normalize_answer,
    predicate_grader,
)
from fastbrowse.evals.live_tasks import Outcome

_PASSING_GRADER = """
import json, os, sys
request = json.loads(sys.stdin.read())
outcome = request["outcome"]
passed = outcome.get("answer") == "42"
print(json.dumps({
    "grader": "fixture-grader",
    "version": "1.2",
    "passed": passed,
    "failure": None if passed else "answer was not 42",
    "evidence": {"task": request["task"]["id"], "environment": sorted(os.environ)},
}))
"""

_SLEEPING_GRADER = """
import os, sys, time
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    handle.write(str(os.getpid()))
time.sleep(30)
"""

_SPAWNING_GRADER = """
import subprocess, sys
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    handle.write(str(child.pid))
"""


def _script(tmp_path: Path, name: str, body: str) -> Path:
    path = tmp_path / name
    path.write_text(body, encoding="utf-8")
    return path


def _ref(predicate: dict | None = None) -> TaskRef:
    return TaskRef(
        source="windtunnel",
        revision="a" * 40,
        sha256="b" * 64,
        id="wt-1",
        stratum="answer",
        start="http://localhost/",
        task="What is the shipping window?",
        metadata={} if predicate is None else {"predicate": predicate},
        digest="c" * 64,
    )


def _outcome(answer: str | None = "42", data: object = None) -> Outcome:
    return Outcome(answer=answer, data=data, final_url="http://localhost/result")


def test_normalize_answer_matches_the_pinned_predicate_rules() -> None:
    # Both the typographic dash and the prose range fold to the same text, as in predicates.mjs.
    assert normalize_answer("Ships in 3\u20135 business days") == "ships in 3 5 business days"
    assert normalize_answer("Ships in 3 to 5 business days") == "ships in 3 5 business days"
    assert normalize_answer("Don\u2019t visit https://example.test/a now") == "don't visit now"
    assert normalize_answer("  a_b-c  ") == "a b c"


def test_answer_predicate_contains_accepts_restated_site_text() -> None:
    grade = grade_answer_predicate(
        _ref({"type": "answer", "contains": ["3\u20135 business days"]}),
        _outcome("It ships in 3 to 5 business days."),
    )
    assert grade.grader == grade_module.NATIVE_GRADER
    assert grade.passed is True and grade.failure is None
    assert grade.evidence["predicate"] == {"type": "answer", "contains": ["3\u20135 business days"]}


def test_answer_predicate_failure_names_the_expectation_but_is_still_graded() -> None:
    grade = grade_answer_predicate(
        _ref({"type": "answer", "contains": ["3 to 5 business days"]}), _outcome("It ships in 2 days.")
    )
    assert grade.passed is False and grade.failure is not None and "answer predicate failed" in grade.failure


def test_answer_predicate_contains_any_matches_and_negation() -> None:
    any_grade = grade_answer_predicate(
        _ref({"type": "answer", "contains_any": ["express", "overnight"]}), _outcome("Overnight shipping only.")
    )
    assert any_grade.passed is True
    missed = grade_answer_predicate(
        _ref({"type": "answer", "contains_any": ["express", "overnight"]}), _outcome("Standard shipping.")
    )
    assert missed.passed is False

    pattern = grade_answer_predicate(_ref({"type": "answer", "matches": r"\b42\b"}), _outcome("The total is 42."))
    assert pattern.passed is True

    negated = grade_answer_predicate(
        _ref({"type": "answer", "contains": ["shipping"], "not_contains": ["free"]}),
        _outcome("Shipping is free."),
    )
    assert negated.passed is False and negated.failure is not None and "negation" in negated.failure


def test_answer_predicate_without_text_or_with_a_bad_pattern_stays_ungraded() -> None:
    with pytest.raises(GraderError, match="no expected text"):
        grade_answer_predicate(_ref({"type": "answer"}), _outcome())
    # An empty pattern is falsy in the pinned scorer too, so it is no expected text rather than a match-everything.
    with pytest.raises(GraderError, match="no expected text"):
        grade_answer_predicate(_ref({"type": "answer", "matches": ""}), _outcome())
    with pytest.raises(GraderError, match="invalid pattern"):
        grade_answer_predicate(_ref({"type": "answer", "matches": "("}), _outcome())


def test_answer_predicate_with_an_unknown_key_stays_ungraded() -> None:
    # An unsupported key is refused rather than ignored: dropping `equals` would grade on `contains` alone.
    with pytest.raises(GraderError, match="malformed answer predicate"):
        grade_answer_predicate(_ref({"type": "answer", "contains": ["42"], "equals": "42"}), _outcome())


def test_action_predicates_and_missing_predicates_stay_ungraded() -> None:
    with pytest.raises(UnsupportedPredicate, match="live state probe"):
        grade_answer_predicate(_ref({"type": "act-short", "assert": {"contains": "x"}}), _outcome())
    with pytest.raises(UnsupportedPredicate, match="no predicate"):
        grade_answer_predicate(_ref(), _outcome())


async def test_local_command_grader_validates_and_pins_identity(tmp_path: Path) -> None:
    script = _script(tmp_path, "grader.py", _PASSING_GRADER)
    grader = local_command_grader([sys.executable, str(script)], code=script)

    grade = await grader(_ref(), _outcome(data={"rows": (1, 2)}))
    assert grade.grader == "fixture-grader"
    assert grade.passed is True and grade.failure is None
    assert grade.version.startswith("1.2+") and grade.version.endswith(grade_module.file_digest(script)[:16])
    assert grade.evidence["task"] == "wt-1"
    assert grade.evidence["grader_code_sha256"] == grade_module.file_digest(script)
    assert grade.evidence["grader_command"] == [sys.executable, str(script)]
    assert grade.evidence["reported_version"] == "1.2"

    failed = await grader(_ref(), _outcome("7"))
    assert failed.passed is False and failed.failure == "answer was not 42"


async def test_local_command_grader_environment_excludes_provider_keys(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "not-a-real-key")
    monkeypatch.setenv("AI_GATEWAY_API_KEY", "not-a-real-key")
    script = _script(tmp_path, "grader.py", _PASSING_GRADER)
    grader = local_command_grader([sys.executable, str(script)], code=script)

    grade = await grader(_ref(), _outcome())
    environment = grade.evidence["environment"]
    assert isinstance(environment, list)
    assert "OPENROUTER_API_KEY" not in environment and "AI_GATEWAY_API_KEY" not in environment
    assert "PATH" in environment


async def test_local_command_grader_timeout_kills_the_process_group(tmp_path: Path) -> None:
    script = _script(tmp_path, "sleep.py", _SLEEPING_GRADER)
    pid_file = tmp_path / "pid"
    grader = local_command_grader([sys.executable, str(script), str(pid_file)], code=script, timeout=0.5)

    with pytest.raises(GraderError, match="exceeded"):
        await grader(_ref(), _outcome())
    with pytest.raises(ProcessLookupError):
        os.kill(int(pid_file.read_text(encoding="utf-8")), 0)


async def test_local_command_grader_kills_the_group_after_the_leader_exits(tmp_path: Path) -> None:
    script = _script(tmp_path, "spawn.py", _SPAWNING_GRADER)
    pid_file = tmp_path / "child"
    grader = local_command_grader([sys.executable, str(script), str(pid_file)], code=script, timeout=0.5)

    with pytest.raises(GraderError, match="exceeded"):
        await grader(_ref(), _outcome())
    child = int(pid_file.read_text(encoding="utf-8"))
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            os.kill(child, 0)
        except ProcessLookupError:
            return
        await asyncio.sleep(0.05)
    pytest.fail("the spawned grandchild survived the process-group kill")


async def test_local_command_grader_rejects_a_failed_command_or_bad_output(tmp_path: Path) -> None:
    failer = _script(tmp_path, "failer.py", "import sys\nsys.stderr.write('boom')\nsys.exit(3)\n")
    with pytest.raises(GraderError, match="exited 3"):
        await local_command_grader([sys.executable, str(failer)], code=failer)(_ref(), _outcome())

    talker = _script(tmp_path, "talker.py", "print('not json')\n")
    with pytest.raises(GraderError, match="not a valid Grade"):
        await local_command_grader([sys.executable, str(talker)], code=talker)(_ref(), _outcome())

    extra = _script(
        tmp_path,
        "extra.py",
        "import json\nprint(json.dumps({'grader': 'g', 'version': '1', 'passed': True, 'extra': 1}))\n",
    )
    with pytest.raises(GraderError, match="not a valid Grade"):
        await local_command_grader([sys.executable, str(extra)], code=extra)(_ref(), _outcome())


async def test_local_command_grader_revalidates_its_digest_each_call(tmp_path: Path) -> None:
    script = _script(tmp_path, "grader.py", _PASSING_GRADER)
    grader = local_command_grader([sys.executable, str(script)], code=script)
    assert (await grader(_ref(), _outcome())).passed is True
    # An evaluator edited after it was pinned must not keep grading under the old digest.
    script.write_text(_PASSING_GRADER + "\n# edited\n", encoding="utf-8")
    with pytest.raises(GraderError, match="changed since it was pinned"):
        await grader(_ref(), _outcome())


def test_local_command_grader_rejects_an_unsafe_configuration(tmp_path: Path) -> None:
    script = _script(tmp_path, "grader.py", _PASSING_GRADER)
    with pytest.raises(ValueError, match="must not be empty"):
        local_command_grader([])
    with pytest.raises(ValueError, match="must be explicit"):
        local_command_grader([sys.executable, str(script)])
    with pytest.raises(ValueError, match="positive number"):
        local_command_grader([sys.executable, str(script)], code=script, timeout=0)
    with pytest.raises(ValueError, match="not a file"):
        local_command_grader([sys.executable], code=tmp_path / "missing.py")
    with pytest.raises(ValueError, match="not found"):
        local_command_grader(["definitely-not-a-real-grader-binary"])
    with pytest.raises(ValueError, match="lowercase sha256"):
        local_command_grader([sys.executable, str(script)], code=script, digest="nope")
    with pytest.raises(ValueError, match="does not match"):
        local_command_grader([sys.executable, str(script)], code=script, digest="0" * 64)
    # A matching pinned digest is accepted and recorded unchanged.
    pinned = grade_module.file_digest(script)
    grader = local_command_grader([sys.executable, str(script)], code=script, digest=pinned)
    assert isinstance(grader, grade_module.LocalCommandGrader) and grader.digest == pinned


async def test_predicate_grader_routes_answers_natively_and_actions_to_the_command(tmp_path: Path) -> None:
    script = _script(tmp_path, "grader.py", _PASSING_GRADER)

    without_command = predicate_grader()
    with pytest.raises(UnsupportedPredicate):
        await without_command(_ref({"type": "act-long", "assert": {}}), _outcome())

    with_command = predicate_grader([sys.executable, str(script)], code=script)
    action = await with_command(_ref({"type": "act-long", "assert": {}}), _outcome())
    assert action.grader == "fixture-grader" and action.passed is True
    answer = await with_command(_ref({"type": "answer", "contains": ["42"]}), _outcome())
    assert answer.grader == grade_module.NATIVE_GRADER and answer.passed is True
