"""The native-state contract: declarative witnesses, a strict supplement, and the grader command that applies it."""

import asyncio
import hashlib
import importlib.util
import json
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest
from pydantic import JsonValue, ValidationError

from fastbrowse.evals.corpus import Grade, TaskRef
from fastbrowse.evals.external_grade import GraderError, local_command_grader
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.native_state import (
    Equals,
    Http,
    Observation,
    Records,
    Supplement,
    TrustedObservations,
    canonical_digest,
    evaluate,
    parse_supplements,
    run_check,
    select_observations,
)

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "native_grader.py"
SUPPLEMENTS = ROOT / "docs" / "validation" / "2026-10-06-native-state-supplements.json"
PREDICATE: dict[str, JsonValue] = {"probe": "database", "assert": {"contains": {"customers": 2}}}
OBSERVER = "e" * 64
OBSERVE = [{"kind": "probe", "name": "state", "probe": "p"}]


def _supplement(**overrides: object) -> Supplement:
    base: dict[str, object] = {
        "id": "s",
        "task_id": "t",
        "version": "1",
        "source": "windtunnel",
        "revision": "a" * 40,
        "dataset_sha256": "b" * 64,
        "site": "s",
        "task_sha256": hashlib.sha256(b"do it").hexdigest(),
        "upstream_sha256": canonical_digest(PREDICATE),
        "purpose": "p",
        "witnesses": [
            {"label": "before", "observe": OBSERVE, "checks": [{"kind": "minimum", "path": "state.n", "value": 1}]},
            {
                "label": "during",
                "observe": OBSERVE,
                "checks": [
                    {
                        "kind": "records",
                        "path": "state.rows",
                        "match": {"who": {"email": "a@x.test"}},
                        "maximum": 1,
                        "casefold": True,
                    }
                ],
            },
            {
                "label": "after",
                "source": "grade",
                "observe": OBSERVE,
                "checks": [{"kind": "equals", "path": "state.n", "value": 2}],
            },
        ],
    }
    return Supplement.model_validate({**base, **overrides})


def _witness(label: str, check: dict[str, object], **extra: object) -> dict[str, object]:
    source = "grade" if label == "after" else "runner"
    return {"label": label, "source": source, "observe": OBSERVE, "checks": [check], **extra}


def _obs(label: str, data: Any) -> Observation:
    return Observation(label=label, data=data)


def _state(label: str, data: Any) -> Observation:
    return _obs(label, {"state": data})


FULL = [
    _state("before", {"n": 1}),
    _state("during", {"rows": [{"who": {"email": "A@x.test"}}]}),
    _state("after", {"n": 2}),
]


def test_a_full_sequence_of_passing_witnesses_passes() -> None:
    verdict = evaluate(_supplement(), FULL)
    assert (verdict.status, verdict.failures, verdict.missing) == ("pass", (), ())


def test_record_matching_folds_case_only_when_asked() -> None:
    def one(**extra: object) -> Supplement:
        check = {"kind": "records", "path": "state.rows", "match": {"k": "V"}, **extra}
        return _supplement(witnesses=[_witness("before", check)])

    data = {"rows": [{"k": "v"}]}
    assert evaluate(one(), [_state("before", data)]).status == "fail"
    assert evaluate(one(casefold=True), [_state("before", data)]).status == "pass"


def test_a_missing_required_witness_is_ungraded_not_passed() -> None:
    verdict = evaluate(_supplement(), [FULL[0], FULL[2]])
    assert (verdict.status, verdict.missing) == ("ungraded", ("during",))


def test_a_failing_witness_beats_a_missing_one() -> None:
    verdict = evaluate(_supplement(), [_state("before", {"n": 0})])
    assert verdict.status == "fail"
    assert verdict.missing == ("during", "after")


def test_an_optional_witness_may_be_absent() -> None:
    supplement = _supplement(
        witnesses=[
            _witness("before", {"kind": "equals", "path": "state.n", "value": 1}, required=False),
            _witness("after", {"kind": "equals", "path": "state.n", "value": 2}),
        ]
    )
    assert evaluate(supplement, [_state("after", {"n": 2})]).status == "pass"


@pytest.mark.parametrize(
    "observations",
    [
        [FULL[1], FULL[0], FULL[2]],  # out of sequence
        [*FULL, _state("after", {"n": 2})],  # the same moment read twice
        [*FULL, _state("extra", {})],  # a reading no witness asked for
        [FULL[0], _state("during", {"rows": [{"who": {"email": "a@x.test"}}] * 2}), FULL[2]],  # too many records
        [FULL[0], _state("during", {"rows": "text"}), FULL[2]],  # not a list
        [FULL[0], FULL[1], _state("after", {})],  # path never observed
        [FULL[0], FULL[1], _state("after", {"n": True})],  # a bool is not the number 2
        [_state("before", {"n": float("nan")}), FULL[1], FULL[2]],  # not a number a bound can hold
    ],
)
def test_contradicting_or_malformed_observations_fail(observations: list[Observation]) -> None:
    assert evaluate(_supplement(), observations).status == "fail"


@pytest.mark.parametrize(
    "overrides",
    [
        {"surprise": 1},
        {"witnesses": [{**_witness("before", {"kind": "equals", "value": 1}), "source": "grade"}]},  # grade-time before
        {"witnesses": [{**_witness("after", {"kind": "equals", "value": 1}), "source": "runner"}]},  # sampled after
        {"witnesses": [{**_witness("before", {"kind": "equals", "value": 1}), "observe": []}]},  # reads nothing
        {"witnesses": [_witness("before", {"kind": "equals", "value": 1})] * 2},  # a label twice
        {"witnesses": [_witness("before", {"kind": "minimum", "value": True})]},  # true is not a bound
        {"witnesses": [_witness("before", {"kind": "maximum", "value": float("inf")})]},
        {"witnesses": [_witness("before", {"kind": "records", "match": {}, "minimum": 2, "maximum": 1})]},
        {"witnesses": [_witness("before", {"kind": "records", "match": {}, "minimum": True})]},
        {"upstream_sha256": "short"},
        {"site": ""},
    ],
)
def test_loose_or_inconsistent_supplements_are_refused(overrides: dict[str, object]) -> None:
    with pytest.raises(ValidationError):
        _supplement(**overrides)


def test_canonical_digest_refuses_what_json_cannot_hold() -> None:
    assert canonical_digest({"b": 1, "a": [1, "x"]}) == canonical_digest({"a": [1, "x"], "b": 1})
    for value in ({"a": object()}, {"a": {1, 2}}, {"a": float("nan")}):
        with pytest.raises(ValueError, match="canonical JSON"):
            canonical_digest(value)


def _trusted(*samples: tuple[str, dict[str, Any]], **extra: object) -> TrustedObservations:
    return TrustedObservations.model_validate(
        {
            "observer_sha256": OBSERVER,
            "task_id": "t",
            "samples": [
                {"sequence": index, "at": float(index), "phase": phase, "data": {"state": data}}
                for index, (phase, data) in enumerate(samples)
            ],
            **extra,
        }
    )


def test_the_observer_samples_pick_the_last_before_and_the_first_during_that_holds() -> None:
    during = {"rows": [{"who": {"email": "a@x.test"}}]}
    trusted = _trusted(
        ("before", {"n": 0}),
        ("before", {"n": 1}),
        ("during", {"rows": []}),
        ("during", during),
        ("during", {"rows": []}),
    )
    found = select_observations(_supplement(), trusted)
    assert [(item.label, item.data) for item in found] == [
        ("before", {"state": {"n": 1}}),
        ("during", {"state": during}),
    ]


def test_a_state_the_sampler_never_caught_is_absent_not_passed() -> None:
    trusted = _trusted(("before", {"n": 1}), ("during", {"rows": []}))
    assert [item.label for item in select_observations(_supplement(), trusted)] == ["before"]


@pytest.mark.parametrize(
    "samples",
    [
        [{"sequence": 1, "at": 0, "phase": "before", "data": {}}],  # does not start at 0
        [
            {"sequence": 0, "at": 2, "phase": "before", "data": {}},
            {"sequence": 1, "at": 1, "phase": "before", "data": {}},  # time runs backwards
        ],
        [
            {"sequence": 0, "at": 0, "phase": "during", "data": {}},
            {"sequence": 1, "at": 1, "phase": "before", "data": {}},  # phase runs backwards
        ],
        [{"sequence": 0, "at": float("inf"), "phase": "before", "data": {}}],
        [{"sequence": 0, "at": True, "phase": "before", "data": {}}],
    ],
)
def test_an_incoherent_sample_sequence_is_refused(samples: list[dict[str, object]]) -> None:
    with pytest.raises(ValidationError):
        TrustedObservations.model_validate({"observer_sha256": OBSERVER, "task_id": "t", "samples": samples})


def test_the_documented_supplement_is_valid_and_bound_to_its_upstream_predicate() -> None:
    supplements = parse_supplements(json.loads(SUPPLEMENTS.read_text(encoding="utf-8")))
    supplement = supplements["ea-7"]
    assert supplement.upstream_sha256 == canonical_digest(PREDICATE)
    assert supplement.labels == ("before", "during", "after")
    assert [w.source for w in supplement.witnesses] == ["runner", "runner", "grade"]
    assert supplement.unwitnessed  # the cancellation reason has no witness
    # The sequence contract is deterministic without a runtime: the shapes below are the probe's real output.
    booked = {"appointments": {"count": 1, "appointments": [_row("Consultation")]}}
    final = {
        "database": {"customers": 2, "appointments": 0},
        "appointments": {"count": 0, "appointments": []},
        "customers": [{"firstName": "Casey", "lastName": "Reed", "email": "casey.reed@example.com"}],
    }
    base = {"database": {"customers": 1, "appointments": 0}}
    good = [_obs("before", base), _obs("during", booked), _obs("after", final)]
    assert evaluate(supplement, good).status == "pass"
    assert evaluate(supplement, [good[0], good[2]]).status == "ungraded"
    uncancelled = {**final, "database": {"customers": 2, "appointments": 1}, "appointments": booked["appointments"]}
    assert evaluate(supplement, [*good[:2], _obs("after", uncancelled)]).status == "fail"
    missing_customer = {**final, "customers": []}
    assert evaluate(supplement, [*good[:2], _obs("after", missing_customer)]).status == "fail"
    doubled = {**final, "appointments": {"count": 2, "appointments": [_row("Consultation")] * 2}}
    assert evaluate(supplement, [*good[:2], _obs("after", doubled)]).status == "fail"


def _row(service: str) -> dict[str, object]:
    return {"service": {"name": service}, "customer": {"email": "casey.reed@example.com"}}


_UPSTREAM = """
import json, sys
request = json.loads(sys.stdin.read())
assert set(request) == {"schema_version", "task", "outcome"}, sorted(request)
passed = request["outcome"]["answer"] == "ok"
print(json.dumps({"grader": "up", "version": "9", "passed": passed, "failure": None if passed else "no",
                  "evidence": {"kept": [1]}}))
"""

_OBSERVER = """
import json, sys
site, probe, args = sys.argv[1:4]
print(json.dumps({"site": site, "probe": probe, "n": 2}))
"""


def _command(tmp_path: Path, spec: dict[str, object]) -> list[str]:
    (tmp_path / "up.py").write_text(_UPSTREAM, encoding="utf-8")
    (tmp_path / "observe.py").write_text(_OBSERVER, encoding="utf-8")
    (tmp_path / "supplements.json").write_text(json.dumps({"supplements": [spec]}), encoding="utf-8")
    template = [sys.executable, str(tmp_path / "observe.py"), "{site}", "{probe}", "{args}"]
    return [
        sys.executable,
        str(SCRIPT),
        "--supplements",
        str(tmp_path / "supplements.json"),
        "--observer-sha256",
        OBSERVER,
        *(item for part in template for item in ("--observe-arg", part)),
        "--upstream-code",
        str(tmp_path / "up.py"),
        "--upstream-code-sha256",
        hashlib.sha256(_UPSTREAM.encode()).hexdigest(),
        "--upstream",
        sys.executable,
        str(tmp_path / "up.py"),
    ]


def _ref(predicate: dict[str, JsonValue] = PREDICATE, task_id: str = "t", **overrides: Any) -> TaskRef:
    fields: dict[str, Any] = {
        "source": "windtunnel",
        "revision": "a" * 40,
        "sha256": "b" * 64,
        "id": task_id,
        "stratum": "transaction",
        "start": "http://localhost/",
        "task": "do it",
        "site": "s",
        "metadata": {"predicate": predicate},
        "digest": "c" * 64,
    }
    return TaskRef(**{**fields, **overrides})


def _spec(**overrides: object) -> dict[str, Any]:
    spec: dict[str, Any] = _supplement().model_dump(mode="json")
    spec["witnesses"] = [
        _witness("during", {"kind": "equals", "path": "state.k", "value": 1}),
        {
            **_witness("after", {"kind": "equals", "path": "state.n", "value": 2}),
            "observe": [{"kind": "probe", "name": "state", "probe": "p", "args": {"x": 1}}],
        },
    ]
    return spec | overrides


def _request(ref: TaskRef, answer: str = "ok", **extra: object) -> dict[str, Any]:
    outcome = {"answer": answer, "data": None, "final_url": "http://localhost/", "unobservable": False}
    return {"schema_version": 1, "task": ref.model_dump(mode="json"), "outcome": outcome, **extra}


def _trusted_key(*samples: tuple[str, dict[str, Any]], **extra: object) -> dict[str, Any]:
    return _trusted(*samples, **extra).model_dump(mode="json")


DURING = ("during", {"k": 1})


def _run(tmp_path: Path, request: dict[str, object], spec: dict[str, object] | None = None):
    """The grader script exactly as the bridge starts it, with the request JSON on stdin."""
    return subprocess.run(
        _command(tmp_path, spec or _spec()),
        input=json.dumps(request).encode(),
        capture_output=True,
        check=False,
    )


def _graded(result: subprocess.CompletedProcess[bytes]) -> dict[str, Any]:
    assert result.returncode == 0, result.stderr.decode()
    return json.loads(result.stdout)


def test_the_observers_samples_and_the_upstream_grade_are_labelled_apart(tmp_path: Path) -> None:
    result = _run(tmp_path, _request(_ref(), native_observations=_trusted_key(("before", {}), DURING)))
    grade = _graded(result)
    assert grade["passed"] is True
    assert grade["grader"] == "windtunnel-native-state+strict-supplement"
    upstream = grade["evidence"]["upstream"]
    assert (upstream["passed"], upstream["grader"], upstream["evidence"]) == (True, "up", {"kept": [1]})
    assert upstream["predicate_sha256"] == canonical_digest(PREDICATE)
    assert grade["evidence"]["strict_supplement"]["status"] == "pass"
    assert grade["evidence"]["strict_supplement"]["label"] == "strict-supplement"


def test_a_sampler_that_only_saw_a_wrong_during_state_leaves_it_ungraded(tmp_path: Path) -> None:
    request = _request(_ref(), native_observations=_trusted_key(("during", {"k": 0})))
    result = _run(tmp_path, request)
    assert result.returncode == 3 and b"required witness not supplied: during" in result.stderr


def test_an_upstream_failure_is_not_rescued_by_the_supplement(tmp_path: Path) -> None:
    grade = _graded(_run(tmp_path, _request(_ref(), "bad", native_observations=_trusted_key(DURING))))
    assert (grade["passed"], grade["failure"]) == (False, "no")


def test_a_grade_time_contradiction_fails_even_with_trusted_samples(tmp_path: Path) -> None:
    spec = _spec()
    spec["witnesses"][1]["checks"][0]["value"] = 3
    grade = _graded(_run(tmp_path, _request(_ref(), native_observations=_trusted_key(DURING)), spec))
    assert grade["passed"] is False
    assert "after" in grade["failure"]


def test_readings_an_agent_put_in_its_own_outcome_are_never_witnesses(tmp_path: Path) -> None:
    request = _request(_ref())
    request["outcome"]["data"] = {"native_observations": [{"label": "during", "data": {"state": {"k": 1}}}]}
    result = _run(tmp_path, request)
    assert result.returncode == 3 and b"required witness not supplied: during" in result.stderr
    # Nor does the same shape at the top level pass without the pinned observer's digest.
    forged = _trusted_key(DURING, observer_sha256="f" * 64)
    result = _run(tmp_path, _request(_ref(), native_observations=forged))
    assert result.returncode == 3 and b"pinned observer" in result.stderr


def test_samples_for_another_task_or_in_a_broken_sequence_leave_it_ungraded(tmp_path: Path) -> None:
    other = _request(_ref(), native_observations=_trusted_key(DURING, task_id="other"))
    assert _run(tmp_path, other).returncode == 3
    broken = _trusted_key(DURING)
    broken["samples"][0]["sequence"] = 4
    assert _run(tmp_path, _request(_ref(), native_observations=broken)).returncode == 3


def test_a_grade_time_observer_that_returns_nothing_leaves_it_ungraded(tmp_path: Path) -> None:
    command = _command(tmp_path, _spec())
    (tmp_path / "observe.py").write_text("import sys; sys.exit(1)", encoding="utf-8")
    result = subprocess.run(
        command,
        input=json.dumps(_request(_ref(), native_observations=_trusted_key(DURING))).encode(),
        capture_output=True,
        check=False,
    )
    assert result.returncode == 3 and b"observer returned nothing" in result.stderr


def test_a_supplement_that_cannot_witness_a_requirement_is_limited_not_passed(tmp_path: Path) -> None:
    spec = _spec(unwitnessed=["the cancellation reason"])
    request = _request(_ref(), native_observations=_trusted_key(DURING))
    result = _run(tmp_path, request, spec)
    assert result.returncode == 3 and b"limited, cannot witness: the cancellation reason" in result.stderr
    # A contradiction still fails: limits only stop a pass.
    spec["witnesses"][1]["checks"][0]["value"] = 3
    assert _graded(_run(tmp_path, request, spec))["passed"] is False


@pytest.mark.parametrize(
    "ref",
    [
        _ref(PREDICATE, task="do something else"),
        _ref(PREDICATE, site="elsewhere"),
        _ref(PREDICATE, sha256="d" * 64),
        _ref(PREDICATE, revision="d" * 40),
        _ref({"probe": "database", "assert": {"contains": {"customers": 3}}}),
    ],
)
def test_a_supplement_is_refused_for_any_other_task_identity(tmp_path: Path, ref: TaskRef) -> None:
    result = _run(tmp_path, _request(ref, native_observations=_trusted_key(DURING)))
    assert result.returncode == 2 and b"supplement pins" in result.stderr


def test_a_task_without_a_supplement_keeps_the_upstream_grade(tmp_path: Path) -> None:
    grade = _graded(_run(tmp_path, _request(_ref(task_id="other"))))
    assert (grade["grader"], grade["passed"]) == ("up", True)


def _bridge(tmp_path: Path, answer: str = "ok"):
    grader = local_command_grader(_command(tmp_path, _spec()), code=SCRIPT)
    outcome = Outcome(answer=answer, data={"native_observations": []}, final_url="http://localhost/")

    async def call() -> Grade:
        return await grader(_ref(), outcome)

    return asyncio.run(call())


def test_through_the_real_bridge_an_ordinary_run_with_no_watcher_is_ungraded(tmp_path: Path) -> None:
    with pytest.raises(GraderError, match="required witness not supplied: during"):
        _bridge(tmp_path)


def test_a_boolean_never_equals_a_number_on_either_side_or_nested() -> None:
    pairs: list[tuple[JsonValue, JsonValue]] = [
        (1, True),
        (True, 1),
        (0, False),
        ([1], [True]),
        ({"k": 1}, {"k": True}),
    ]
    for expected, actual in pairs:
        assert run_check(Equals(kind="equals", value=expected), actual) is not None, (expected, actual)
    assert run_check(Equals(kind="equals", value={"k": [True]}), {"k": [True]}) is None
    nested = Records(kind="records", match={"flags": {"paid": 1}})
    assert run_check(nested, [{"flags": {"paid": True}}]) is not None
    assert run_check(nested, [{"flags": {"paid": 1}}]) is None


def test_witnesses_must_be_declared_before_during_after() -> None:
    check: dict[str, object] = {"kind": "equals", "value": 1}
    with pytest.raises(ValidationError, match="order"):
        _supplement(witnesses=[_witness("after", check), _witness("before", check)])
    with pytest.raises(ValidationError, match="order"):
        _supplement(witnesses=[_witness("during", check), _witness("before", check)])
    _supplement(witnesses=[_witness("before", check), _witness("after", check)])


def _flag(command: list[str], flag: str) -> list[str]:
    at = command.index(flag)
    return command[:at] + command[at + 2 :]


def _graded_command(tmp_path: Path) -> list[str]:
    return _command(tmp_path, _spec())


def _stdin() -> bytes:
    return json.dumps(_request(_ref(), native_observations=_trusted_key(DURING))).encode()


def _try(command: list[str]) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(command, input=_stdin(), capture_output=True, check=False)


def test_grade_mode_requires_a_paired_real_upstream_pin_before_reading_anything(tmp_path: Path) -> None:
    command = _graded_command(tmp_path)
    for broken in (_flag(command, "--upstream-code"), _flag(command, "--upstream-code-sha256")):
        result = _try(broken)
        assert result.returncode == 2 and b"needs both" in result.stderr
    wrong = command.copy()
    wrong[wrong.index("--upstream-code-sha256") + 1] = "0" * 64
    assert b"does not match its pin" in _try(wrong).stderr
    malformed = command.copy()
    malformed[malformed.index("--upstream-code-sha256") + 1] = "ABC"
    assert b"hex digest" in _try(malformed).stderr
    other = tmp_path / "other.py"
    other.write_text("print(1)", encoding="utf-8")
    elsewhere = command.copy()
    elsewhere[elsewhere.index("--upstream-code") + 1] = str(other)
    elsewhere[elsewhere.index("--upstream-code-sha256") + 1] = hashlib.sha256(other.read_bytes()).hexdigest()
    assert b"command runs" in _try(elsewhere).stderr
    assert _try(command).returncode == 0


def test_the_grade_identity_carries_the_supplements_content_digest(tmp_path: Path) -> None:
    first = _graded(_run(tmp_path, _request(_ref(), native_observations=_trusted_key(DURING))))["version"]
    spec = _spec()
    digest = canonical_digest(Supplement.model_validate(spec).model_dump(mode="json"))[:16]
    assert first.endswith(f".{digest}")
    spec["witnesses"][1]["checks"][0]["value"] = 3  # same version string, different strict check
    second = _graded(_run(tmp_path, _request(_ref(), native_observations=_trusted_key(DURING)), spec))["version"]
    assert second != first


def test_a_base_url_with_a_path_or_credentials_is_refused(tmp_path: Path) -> None:
    command = _graded_command(tmp_path)
    for bad in ("http://127.0.0.1:1/api", "http://user:pw@127.0.0.1:1", "ftp://127.0.0.1"):
        (tmp_path / "bases.json").write_text(json.dumps({"s": bad}), encoding="utf-8")
        result = _try([*command[:2], "--base-urls", str(tmp_path / "bases.json"), *command[2:]])
        assert result.returncode == 2 and b"cannot read configuration" in result.stderr


def test_an_http_observer_follows_no_redirect_so_the_credential_stays_on_its_origin() -> None:
    hits = {"origin": [], "trap": []}

    class Trap(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            hits["trap"].append(self.headers.get("Authorization"))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"{}")

        def log_message(self, format: str, *args: object) -> None:
            pass

    trap = ThreadingHTTPServer(("127.0.0.1", 0), Trap)

    class Origin(Trap):
        def do_GET(self) -> None:
            hits["origin"].append(self.headers.get("Authorization"))
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{trap.server_port}/steal")
            self.end_headers()

    origin = ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    for server in (trap, origin):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        loaded = importlib.util.spec_from_file_location("native_grader_script", SCRIPT)
        assert loaded is not None and loaded.loader is not None
        module = importlib.util.module_from_spec(loaded)
        loaded.loader.exec_module(module)
        observer = Http(kind="http", name="n", path="/state")
        with pytest.raises(module.Ungraded):
            module.read_http(observer, "s", {"s": f"http://127.0.0.1:{origin.server_port}"}, "user:pw")
    finally:
        for server in (trap, origin):
            server.shutdown()
            server.server_close()
    assert len(hits["origin"]) == 1 and hits["origin"][0] is not None  # the credential did go to its own origin
    assert hits["trap"] == []
