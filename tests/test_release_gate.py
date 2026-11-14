"""`scripts/release_gate.py`: a release passes only when no task got worse and runs got faster and cheaper."""

import importlib.util
import json
import sys
from pathlib import Path

import pytest

_spec = importlib.util.spec_from_file_location(
    "release_gate", Path(__file__).parent.parent / "scripts" / "release_gate.py"
)
assert _spec and _spec.loader
gate = importlib.util.module_from_spec(_spec)
sys.modules["release_gate"] = gate
_spec.loader.exec_module(gate)


def row(
    task: str, passed: bool, seconds: float = 10.0, dollars: float = 0.02, version: int = 1, arm: str = "fastbrowse"
):
    return {
        "arm": arm,
        "suite": "core",
        "task": task,
        "task_version": version,
        "passed": passed,
        "seconds": seconds,
        "dollars": dollars,
    }


BASELINE = [row("a", True), row("a", False), row("b", True), row("b", True)]


def test_a_faster_cheaper_run_with_no_task_worse_passes() -> None:
    candidate = [row("a", True, 8, 0.01), row("a", True, 8, 0.01), row("b", True, 8, 0.01), row("b", True, 8, 0.01)]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == []


def test_a_task_that_passes_less_often_fails_the_release() -> None:
    candidate = [row("a", True, 8, 0.01), row("a", False, 8, 0.01), row("b", True, 8, 0.01), row("b", False, 8, 0.01)]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == ["core/b: passed 1/2, the baseline 2/2"]


def test_a_run_no_faster_or_cheaper_fails_the_release() -> None:
    failures, _ = gate.compare(BASELINE, BASELINE)
    assert [failure.split()[0] for failure in failures] == ["median", "mean"]


def test_a_task_left_out_fails_but_a_new_version_is_only_noted() -> None:
    candidate = [row("a", True, 8, 0.01), row("b", True, 8, 0.01, version=2), row("b", True, 8, 0.01, version=2)]
    failures, notes = gate.compare(candidate, [*BASELINE, row("c", True)])
    assert failures == ["core/c: not run"]
    assert "core/b: version 1 -> 2, not compared" in notes


def test_each_task_is_scored_on_the_newest_campaign_run_that_measured_it_three_times(monkeypatch) -> None:
    def measured(sha: str, task: str, passed: bool, times: int = 3):
        return [{**row(task, passed), "run": {"git_sha": sha}} for _ in range(times)]

    ledgers = {
        3: measured("cccccccc", "a", True, times=1),  # a smoke: too few runs to score
        2: measured("bbbbbbbb", "a", True),  # the targeted rerun of a fix
        1: [*measured("aaaaaaaa", "a", False), *measured("aaaaaaaa", "b", True)],  # the full campaign
        0: [{**r, "task_version": 0} for r in measured("99999999", "b", False)],  # b before its task changed
    }
    titles = {4: "release-9.9.9", 3: "release-1.0.0", 2: "release-1.0.0", 1: "release-1.0.0", 0: "release-1.0.0"}
    runs = [{"databaseId": i, "displayTitle": f"Benchmark campaign {name}"} for i, name in titles.items()]
    monkeypatch.setattr(gate, "_runs", lambda *_: runs)
    monkeypatch.setattr(gate, "run_rows", lambda _, run_id: ledgers[run_id])
    rows, used = gate.campaign_rows("o/r", "release-1.0.0")
    assert {(r["task"], r["run"]["git_sha"], r["passed"]) for r in rows} == {
        ("a", "bbbbbbbb", True),
        ("b", "aaaaaaaa", True),
    }
    assert used == ["run 2 on bbbbbbbb: 1 tasks", "run 1 on aaaaaaaa: 1 tasks"]


def test_run_rows_keeps_two_groups_whose_runs_share_a_run_id(monkeypatch, tmp_path) -> None:
    monkeypatch.syspath_prepend(str(Path(__file__).parent.parent / "evals" / "src"))
    import fastbrowse_evals.publication as publication
    from parallax.models import Verdict

    class TemporaryDirectory:
        def __enter__(self):
            return str(tmp_path)

        def __exit__(self, *_):
            return None

    class Result:
        key = "pypi-version-0"
        task_id = "pypi-version"
        task_version = 1
        repeat = 0
        verdict = Verdict.PASS

    class Loaded:
        def __init__(self, path):
            self.path = path
            self.results = (Result(),)

        def physical_runs(self, _key):
            return self.results

    class Projected:
        def __init__(self, path):
            self.path = path
            self.dollars = 0.0

        def model_dump(self, mode):
            return {"arm": "fastbrowse", "task": "pypi-version", "repeat": 0, "dollars": 0.0}

    run_dirs = (tmp_path / "group-one", tmp_path / "group-two")
    for directory, sha in zip(run_dirs, ("a" * 40, "b" * 40), strict=False):
        directory.mkdir()
        (directory / "manifest.json").write_text(json.dumps({"run_id": "fastbrowse"}))
        (directory / "results.jsonl").write_text("{}\n")
        (directory / "parallax-verify.json").write_text("{}\n")
        (directory / "provenance.json").write_text(json.dumps({"extra": {"fastbrowse.agent_sha": sha}}))

    monkeypatch.setattr(gate.tempfile, "TemporaryDirectory", lambda: TemporaryDirectory())
    monkeypatch.setattr(gate, "_gh", lambda *args: "")
    monkeypatch.setattr("parallax.track.rows.load_run", lambda path: Loaded(path))
    monkeypatch.setattr(publication, "public_arm", lambda result: "fastbrowse")
    monkeypatch.setattr(publication, "trial_result_to_row", lambda result, attempts: Projected(attempts[0]))

    rows = gate.run_rows("owner/repo", 1)

    assert {row["run"]["git_sha"] for row in rows} == {"a" * 40, "b" * 40}


def test_other_arms_are_not_the_release() -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")] + [
        row("a", False, 99, 9, arm="browser-use")
    ]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == []


@pytest.mark.parametrize("side", ["candidate", "baseline"])
@pytest.mark.parametrize("receipt", [{"dollars": None}, {"dollars": 0.0, "unknown_cost": True}])
def test_an_unpriced_attempt_cannot_prove_a_cheaper_release(side, receipt) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    baseline = [dict(r) for r in BASELINE]
    (candidate if side == "candidate" else baseline)[0].update(receipt)
    failures, notes = gate.compare(candidate, baseline)
    assert any(side in failure and "dollars" in failure for failure in failures)
    assert not any(note.startswith("mean $") for note in notes)


def test_a_known_zero_cost_is_still_a_valid_receipt() -> None:
    candidate = [row(task, True, 8, 0) for task in ("a", "a", "b", "b")]
    failures, notes = gate.compare(candidate, BASELINE)
    assert failures == []
    assert any(note.startswith("mean $ 0,") for note in notes)


@pytest.mark.parametrize("side", ["candidate", "baseline"])
@pytest.mark.parametrize("field", ["seconds", "dollars"])
@pytest.mark.parametrize("value", [True, "0.001", float("nan"), float("inf"), -1])
def test_invalid_measurements_cannot_pass_the_release(side, field, value) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    baseline = [dict(r) for r in BASELINE]
    (candidate if side == "candidate" else baseline)[0][field] = value
    failures, _ = gate.compare(candidate, baseline)
    assert any(side in failure and field in failure for failure in failures)


@pytest.mark.parametrize("field", ["seconds", "dollars"])
def test_a_missing_measurement_cannot_pass_the_release(field) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    candidate[0].pop(field)
    failures, _ = gate.compare(candidate, BASELINE)
    assert any("candidate" in failure and field in failure for failure in failures)


def test_an_empty_baseline_cannot_prove_an_improvement() -> None:
    failures, _ = gate.compare(BASELINE, [])
    assert failures == [
        "baseline seconds: missing, unknown or invalid measurements",
        "baseline dollars: missing, unknown or invalid measurements",
    ]


def test_release_speed_never_compares_mixed_clocks():
    baseline = [{**r, "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    candidate = [{**row(r["task"], True, 8, 0.01), "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    candidate[0]["timing_basis"] = "physical-attempt-wall-time"
    failures, notes = gate.compare(candidate, baseline)
    assert failures == ["median seconds cannot compare different timing bases"]
    assert not any(note.startswith("median seconds") for note in notes)


def test_matched_physical_clocks_keep_all_elapsed_time():
    baseline = [{**r, "timing_basis": "physical-attempt-wall-time"} for r in BASELINE]
    candidate = [{**row(r["task"], True, 8, 0.01), "timing_basis": "physical-attempt-wall-time"} for r in BASELINE]
    candidate[0]["seconds"] = 340.3
    failures, _ = gate.compare(candidate, baseline)
    assert failures == []


def test_release_speed_requires_legacy_clock_to_match():
    candidate = [{**r, "dollars": 0.01, "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == ["median seconds cannot compare different timing bases"]


def test_minor_release_requires_both_official_groups():
    import json

    raw = json.loads((Path(__file__).parent.parent / "docs/results/benchmark-candidate.json").read_text())
    with pytest.raises(gate.GateError, match="official groups"):
        gate.official_candidate(raw, raw["campaign_id"], raw["agent_sha"])


def official_raw():
    import json

    raw = json.loads((Path(__file__).parent.parent / "docs/results/benchmark-candidate.json").read_text())
    raw["schema_version"] = 7
    for group in raw["groups"]:
        for row in group["rows"]:
            row["timing_basis"] = "session-ready-to-answer"
    template = raw["groups"][0]["rows"][0]
    for name, count, metric in (("bu-bench", 200, "weighted-rubric"), ("online-mind2web", 300, "full-task-success")):
        tasks = [f"task{i}" for i in range(count)]
        raw["groups"].append(
            {
                "id": name,
                "agent_sha": raw["agent_sha"],
                "runner_sha": raw["runner_sha"],
                "metric": metric,
                "source_sha256": "f" * 64,
                "task_ids": tasks,
                "repeats": 3,
                "limits": ["Test protocol"],
                "sources": [
                    {
                        "run_id": f"synthetic-{name}",
                        "arm": "fastbrowse",
                        "agent_sha": raw["agent_sha"],
                        "runner_sha": raw["runner_sha"],
                        "measured_on": "2026-11-13",
                        "tasks": tasks.copy(),
                        "version": "upstream",
                        "models": ["test:model"],
                    }
                ],
                "rows": [{**template, "task": task, "repeat": repeat} for task in tasks for repeat in range(3)],
            }
        )
    return raw


def check_official(raw):
    return gate.official_candidate(raw, raw["campaign_id"], raw["agent_sha"])


def test_complete_official_coverage_keeps_upstream_partial_scores():
    raw = official_raw()
    raw["groups"][-2]["rows"][0]["score"] = 0.25
    result = check_official(raw)
    assert result.groups[-2].rows[0].score == 0.25
    assert sum(len(g.rows) for g in result.groups[-2:]) == 1500


@pytest.mark.parametrize("repeats", [1, 2, 4])
def test_official_release_requires_three_complete_repeats(repeats):
    raw = official_raw()
    group = raw["groups"][-1]
    group["repeats"] = repeats
    template = group["rows"][0]
    group["rows"] = [{**template, "task": task, "repeat": i} for task in group["task_ids"] for i in range(repeats)]
    with pytest.raises(gate.GateError, match="three repeats"):
        check_official(raw)


@pytest.mark.parametrize("mutation", ["missing", "duplicate", "extra", "foreign-task"])
def test_official_release_refuses_inexact_slots(mutation):
    from pydantic import ValidationError

    raw = official_raw()
    rows = raw["groups"][-1]["rows"]
    if mutation == "missing":
        rows.pop()
    elif mutation == "duplicate":
        rows[-1] = dict(rows[0])
    elif mutation == "extra":
        rows.append({**rows[0], "repeat": 3})
    else:
        for row in rows[-3:]:
            row["task"] = "unrelated"
        raw["groups"][-1]["sources"][0]["tasks"][-1] = "unrelated"
    with pytest.raises(ValidationError, match="scheduled slot"):
        check_official(raw)


def test_official_release_refuses_unknown_cost():
    from pydantic import ValidationError

    raw = official_raw()
    raw["groups"][-1]["rows"][0]["dollars"] = None
    with pytest.raises(ValidationError, match="report their cost"):
        check_official(raw)


@pytest.mark.parametrize("field", ["campaign_id", "agent_sha"])
def test_official_release_requires_exact_candidate_identity(field):
    raw = official_raw()
    kwargs = {"campaign": raw["campaign_id"], "agent_sha": raw["agent_sha"]}
    kwargs["campaign" if field == "campaign_id" else field] = "unrelated"
    with pytest.raises(gate.GateError, match="exact fixture receipt"):
        gate.official_candidate(raw, **kwargs)


@pytest.mark.parametrize("reason", ["no-verdict", "task-changed"])
def test_official_release_does_not_accept_nonproducer_unjudged_reasons(reason):
    raw = official_raw()
    group = raw["groups"][-1]
    removed = group["rows"].pop()
    group["unjudged"] = [{"task": removed["task"], "repeat": removed["repeat"], "arm": "fastbrowse", "reason": reason}]
    with pytest.raises(gate.GateError, match="evidence-unavailable"):
        check_official(raw)


def test_official_release_preserves_evidenced_unjudged_slot():
    raw = official_raw()
    group = raw["groups"][-1]
    removed = group["rows"].pop()
    group["unjudged"] = [
        {"task": removed["task"], "repeat": removed["repeat"], "arm": "fastbrowse", "reason": "evidence-unavailable"}
    ]
    candidate = check_official(raw)
    assert len(candidate.groups[-1].rows) == 899
    assert len(candidate.groups[-1].unjudged) == 1


def test_candidate_intake_requires_successful_producer_stage(monkeypatch):
    import json

    raw = official_raw()
    campaign = raw["campaign_id"]
    monkeypatch.setattr(gate, "_runs", lambda *_: [{"displayTitle": f"Benchmark campaign {campaign}", "databaseId": 1}])

    def gh(*args):
        if args[-1].endswith("/jobs?per_page=100"):
            return json.dumps({"jobs": [{"name": "Build the candidate", "conclusion": "failure"}]})
        if args[0] == "api":
            return json.dumps({"path": ".github/workflows/benchmark-campaign.yml", "head_sha": raw["runner_sha"]})
        pytest.fail("failed producer must not supply an artifact")

    monkeypatch.setattr(gate, "_gh", gh)
    with pytest.raises(gate.GateError, match="no complete official campaign candidate"):
        gate.campaign_candidate("o/r", campaign, "tree", raw["fixture"]["run_id"])


def test_complete_candidate_intake_requires_original_official_receipts(monkeypatch):
    import json

    raw = official_raw()
    campaign = raw["campaign_id"]
    monkeypatch.setattr(gate, "_runs", lambda *_: [{"displayTitle": f"Benchmark campaign {campaign}", "databaseId": 1}])
    monkeypatch.setattr(gate, "_tree", lambda *_: "tree")
    monkeypatch.setattr(gate, "_fixture_receipt", lambda *_: raw["fixture"])

    def gh(*args):
        if args[0] == "run":
            directory = Path(args[args.index("-D") + 1])
            (directory / "benchmark-candidate.json").write_text(json.dumps(raw))
            return ""
        if args[-1].endswith("/jobs?per_page=100"):
            return json.dumps({"jobs": [{"name": "Build the candidate", "conclusion": "success"}]})
        return json.dumps({"path": ".github/workflows/benchmark-campaign.yml", "head_sha": raw["runner_sha"]})

    def refuse(*_):
        raise gate.GateError("missing original official receipts")

    monkeypatch.setattr(gate, "_gh", gh)
    monkeypatch.setattr(gate, "official_receipts", refuse)
    with pytest.raises(gate.GateError, match="missing original official receipts"):
        gate.campaign_candidate("o/r", campaign, "tree", raw["fixture"]["run_id"])


def test_candidate_uses_the_checked_fixture_after_new_same_tree_dispatch(monkeypatch):
    import json

    raw = official_raw()
    campaign = raw["campaign_id"]
    original = raw["fixture"]["run_id"]
    monkeypatch.setattr(gate, "_runs", lambda *_: [{"displayTitle": f"Benchmark campaign {campaign}", "databaseId": 1}])
    monkeypatch.setattr(gate, "_tree", lambda *_: "tree")
    checked = {**raw["fixture"], "run_id": original + 1}
    monkeypatch.setattr(gate, "_fixture_receipt", lambda *_: checked)

    def gh(*args):
        if args[0] == "run":
            directory = Path(args[args.index("-D") + 1])
            (directory / "benchmark-candidate.json").write_text(json.dumps(raw))
            return ""
        if args[-1].endswith("/jobs?per_page=100"):
            return json.dumps({"jobs": [{"name": "Build the candidate", "conclusion": "success"}]})
        return json.dumps({"path": ".github/workflows/benchmark-campaign.yml", "head_sha": "different-workflow-head"})

    monkeypatch.setattr(gate, "_gh", gh)
    monkeypatch.setattr(gate, "official_receipts", lambda *_: ["verified original rows"])
    candidate, notes = gate.campaign_candidate("o/r", campaign, "tree", original + 1)
    assert candidate.fixture.run_id == original + 1
    assert "verified original rows" in notes


def write_baseline(tmp_path, campaign: str):
    """A release-baseline.json naming `campaign`, as docs/results/ holds."""
    path = tmp_path / "release-baseline.json"
    path.write_text(
        json.dumps({"version": "9.8.0", "campaign": campaign, "tasks": ["core/a", "core/b"]}), encoding="utf-8"
    )
    return path


@pytest.mark.parametrize("tag", ["v9.9.0", "v9.9.1"])
@pytest.mark.parametrize(("lines", "official"), [([], False), (["Benchmarks: official"], True)])
def test_official_coverage_is_required_only_when_the_tag_claims_it(monkeypatch, tmp_path, tag, lines, official):
    asked = []
    monkeypatch.setattr(gate, "_tree", lambda *_: "tree")
    monkeypatch.setattr(gate, "fixture_run", lambda *_: 7)
    monkeypatch.setattr(gate, "tag_lines", lambda *_: lines)
    monkeypatch.setattr(gate, "campaign_candidate", lambda *_: asked.append(True) or (None, ["official receipts"]))
    candidate = [row(task, True, 5.0, 0.01) for task in ("a", "b")]
    campaigns = {f"release-{tag[1:]}": (candidate, ["run 1"]), "release-0.10.0": (BASELINE, ["run 0"])}
    monkeypatch.setattr(gate, "campaign_rows", lambda _repo, campaign: campaigns[campaign])
    monkeypatch.setattr(gate, "BASELINE", write_baseline(tmp_path, "release-0.10.0"))
    assert gate.main([tag]) == 0
    # A patch that claims official scores is measured too, so its claim is checked.
    assert asked == ([True] if official else [])


def test_a_baseline_whose_campaign_lost_a_task_refuses_to_compare(monkeypatch) -> None:
    # Run 1's ledger expired, so only run 2's task "a" is left; comparing on "a" alone would skip "b" unseen.
    monkeypatch.setattr(gate, "campaign_rows", lambda *_: ([row("a", True)] * 3, ["run 2"]))
    with pytest.raises(gate.GateError, match="no longer supplies 1 baseline tasks: core/b"):
        gate.baseline_rows("o/r", {"campaign": "release-0.10.0", "tasks": ["core/a", "core/b"]})
