"""The publication gate: integrity refuses, regression is matched and bounded, and noise never blocks.

Each test builds the smallest rows and ledger that make one finding true, so a failure names the rule it broke
rather than a fixture that drifted.
"""

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.evals import catalog as live
from fastbrowse.evals import publication, versions
from fastbrowse.evals.publication import REFUSE, REGRESS
from fastbrowse.models import Limits

TASK = "pypi-version"
SUITE = "core"
RELEASE = "9.9.9"
CORE_SUITE_VERSION = versions.suite_version(t.id for t in live.SUITES["core"])
TASK_VERSION = versions.task_version(TASK) or 0


def row(**overrides: Any) -> dict[str, Any]:
    """A live lookup row that clears every integrity check until an override breaks one."""
    base: dict[str, Any] = {
        "arm": "fastbrowse",
        "task": TASK,
        "category": "lookup",
        "suite": SUITE,
        "suite_version": CORE_SUITE_VERSION,
        "task_version": TASK_VERSION,
        "status": "complete",
        "normalized_status": "done",
        "passed": True,
        "correct": True,
        "seconds": 10.0,
        "dollars": 0.01,
        "unknown_cost": False,
        "retries": 0,
        "repeat": 0,
        "run": {
            "run_id": "run-1",
            "run_started": "2026-10-05T10:00:00+00:00",
            "fastbrowse_version": RELEASE,
            "git_sha": "0" * 40,
            "git_dirty": False,
            "benchmark_catalog_sha256": live.CATALOG_SHA256,
            "benchmark_family": "browser-use",
            "benchmark_origin": "fastbrowse-internal",
            "benchmark_runner_sha": "1" * 40,
            "benchmark_runner_dirty": False,
            "providers": "openrouter",
            "max_steps": 50,
            "concurrency": 2,
            "arms": {"fastbrowse": {"pin": "0.5.18+0123456789", "tier": "A"}},
        },
    }
    return base | overrides


def selected(source: dict[str, Any], *, retries: int | None = None) -> dict[str, Any]:
    return {
        "selected": True,
        "retry_wait_seconds": 0,
        "arm": source["arm"],
        "task": source["task"],
        "repeat": source["repeat"],
        "retries": source["retries"] if retries is None else retries,
        "status": source["status"],
        "normalized_status": source["normalized_status"],
        "seconds": source["seconds"],
        "dollars": source["dollars"],
        "run": source["run"],
    }


def retried(source: dict[str, Any], *, wait: float, seconds: float, dollars: float | None) -> dict[str, Any]:
    """An earlier outage attempt, retained but not selected, as the live harness writes it."""
    entry = selected(source, retries=0) | {
        "selected": False,
        "retry_wait_seconds": wait,
        "retries": 0,
        "normalized_status": "unavailable",
        "seconds": seconds,
        "dollars": dollars,
    }
    return entry


def baseline_row(
    *,
    passed: bool,
    task_version: int = TASK_VERSION,
    seconds: float = 10.0,
    dollars: float | None = 0.01,
    release: str = "9.9.8",
    providers: str = "openrouter",
) -> dict[str, Any]:
    """A published fastbrowse row at an earlier release, with the same protocol and route as `row`."""
    return {
        "arm": "fastbrowse",
        "suite": SUITE,
        "task": TASK,
        "task_version": task_version,
        "normalized_status": "done",
        "passed": passed,
        "seconds": seconds,
        "dollars": dollars,
        "run": {"fastbrowse_version": release, "providers": providers, "max_steps": 50},
    }


def attempts(count: int, **overrides: Any) -> list[dict[str, Any]]:
    """`count` distinct repeats, so each candidate row has its own ledger key."""
    return [row(repeat=index, **overrides) for index in range(count)]


def evidence(rows_: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [selected(source) for source in rows_]


_NAVIGATION_PAGE = {
    "status": 200,
    "title": "example",
    "text": "hello",
    "text_length": 5,
    "inner_width": 1120,
    "inner_height": 780,
    "device_pixel_ratio": 1.0,
}


def suite_rows(suite: str = SUITE, repeats: int = 3) -> list[dict[str, Any]]:
    """Every task of a live suite at `repeats`, so a publication claim covers the suite whole.

    A navigation task passes only with the viewport the comparison was measured at, so those rows carry it.
    """
    tasks = live.SUITES[suite]
    suite_version = versions.suite_version(task.id for task in tasks)
    return [
        row(
            task=task.id,
            category=task.category.value,
            suite=suite,
            suite_version=suite_version,
            task_version=versions.task_version(task.id),
            repeat=repeat,
            final_page=_NAVIGATION_PAGE if task.category.value == "navigate" else None,
        )
        for repeat in range(repeats)
        for task in tasks
    ]


def _at_release(rows_: list[dict[str, Any]], release: str) -> list[dict[str, Any]]:
    """The same rows as published under another release, so a baseline can precede the candidate."""
    return [row | {"run": row["run"] | {"fastbrowse_version": release}} for row in rows_]


# A unit shape is one task, not a published suite, so it opts out of the full-suite coverage check while still
# checking every ledger rule. `validate_diff` and `versions.publish` leave coverage on.
TINY: dict[str, Any] = {"require_ledger": True, "require_coverage": False}


def test_a_clean_live_run_clears_the_gate() -> None:
    sources = attempts(3)
    report = publication.gate(sources, release=RELEASE, ledger=evidence(sources), **TINY)
    assert report.blocking == ()


@pytest.mark.parametrize(
    "field,value",
    [
        ("benchmark_catalog_sha256", "0" * 64),
        ("benchmark_runner_sha", None),
        ("benchmark_runner_dirty", True),
        ("benchmark_origin", "upstream"),
    ],
)
def test_publication_refuses_unmatched_private_runner_provenance(field: str, value: object) -> None:
    sources = attempts(3)
    for source in sources:
        source["run"][field] = value
    report = publication.gate(sources, release=RELEASE, ledger=evidence(sources), **TINY)
    assert any(f.check == "catalog" for f in report.blocking)


def test_publication_refuses_rows_with_no_attempt_ledger() -> None:
    report = publication.gate(attempts(3), release=RELEASE, **TINY)
    assert [finding.check for finding in report.blocking] == ["ledger"]


def test_a_new_live_task_needs_three_distinct_repeats_before_any_baseline() -> None:
    source = row()
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], require_ledger=True)
    assert any(
        finding.severity == REGRESS and "distinct measured repeats" in finding.detail for finding in report.blocking
    )


def test_a_fixture_run_is_not_gated_for_comparison_repeats() -> None:
    from fastbrowse.evals.mock_tasks import TASKS as MOCK

    task = MOCK[0]
    fixture = row(
        task=task.id,
        suite="mock",
        suite_version=versions.suite_version(t.id for t in MOCK),
        task_version=versions.task_version(task.id),
    )
    report = publication.gate([fixture], release=RELEASE, ledger=[selected(fixture)], require_ledger=True)
    assert not any(finding.check == "regression" for finding in report.blocking)


def test_an_unknown_suite_is_refused() -> None:
    """A suite this build does not define has no task list or version to check, so it is refused not reported."""
    source = row(suite="not-a-suite")
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], **TINY)
    assert any(finding.check == "version" and finding.severity == REFUSE for finding in report.blocking)


def test_a_competitor_arm_needs_three_distinct_measured_repeats() -> None:
    rows_ = [_hosted(arm="browser-use", repeat=index) for index in range(2)]
    report = publication.gate(rows_, release=RELEASE, ledger=evidence(rows_))
    assert any(
        finding.severity == REGRESS and "distinct measured repeats" in finding.detail for finding in report.blocking
    )


def test_duplicate_repeats_do_not_make_a_rate() -> None:
    source = row(repeat=0)
    duplicate = row(repeat=0)
    report = publication.gate(
        [source, duplicate], release=RELEASE, ledger=[selected(source), selected(duplicate)], require_ledger=True
    )
    assert any("appears more than once" in finding.detail for finding in report.blocking)


def test_a_row_reporting_retries_without_a_ledger_is_refused() -> None:
    report = publication.gate([row(retries=1)], release=RELEASE)
    assert "retries but no attempt ledger" in report.blocking[0].detail


def test_a_hidden_retry_is_refused_when_the_ledger_misses_an_attempt() -> None:
    source = row(retries=1)
    # Two attempts ran, but the ledger kept only the selected one: the row claims a retry the ledger cannot show.
    report = publication.gate([source], release=RELEASE, ledger=[selected(source, retries=1)], require_ledger=True)
    assert any("physical attempts" in finding.detail for finding in report.blocking)


def test_a_retained_retry_clears_the_gate_and_counts_its_spend() -> None:
    sources = [row(repeat=index, retries=1) for index in range(3)]
    ledger: list[dict[str, Any]] = []
    for source in sources:
        ledger.append(retried(source, wait=60.0, seconds=30.0, dollars=0.005))
        ledger.append(selected(source, retries=1))
    report = publication.gate(sources, release=RELEASE, ledger=ledger, **TINY)
    assert report.blocking == ()
    assert report.operational.retries == 3
    assert report.operational.ledger_attempts == 6
    assert report.operational.retry_dollars == 0.015


def test_an_unpriced_retry_keeps_the_retry_total_unknown() -> None:
    source = row(retries=1)
    ledger = [retried(source, wait=60.0, seconds=30.0, dollars=None), selected(source, retries=1)]
    report = publication.gate([source], release=RELEASE, ledger=ledger, require_ledger=True)
    assert report.operational.retry_dollars is None


def test_a_selected_ledger_attempt_no_row_reports_is_refused() -> None:
    source = row()
    stranger = selected(source) | {"task": "hn-top"}
    report = publication.gate([source], release=RELEASE, ledger=[selected(source), stranger], require_ledger=True)
    assert any("no row reports" in finding.detail for finding in report.blocking)


def test_a_ledger_attempt_from_another_run_is_refused() -> None:
    source = row()
    other = selected(source) | {"run": source["run"] | {"run_id": "run-2"}}
    report = publication.gate([source], release=RELEASE, ledger=[other], require_ledger=True)
    assert any("another run" in finding.detail for finding in report.blocking)


def test_an_orphan_ledger_attempt_is_refused_even_unselected() -> None:
    """An unselected entry with no row is spend and an outage no publication accounts for, so it is refused."""
    source = row()
    orphan = retried(source, wait=0.0, seconds=1.0, dollars=None) | {"task": "hn-top", "selected": False}
    report = publication.gate([source], release=RELEASE, ledger=[selected(source), orphan], require_ledger=True)
    assert any("no row reports" in finding.detail for finding in report.blocking)


def test_a_retry_from_a_different_build_is_refused() -> None:
    source = row(retries=1)
    bad = retried(source, wait=1.0, seconds=1.0, dollars=0.001) | {"run": source["run"] | {"git_sha": "f" * 40}}
    report = publication.gate([source], release=RELEASE, ledger=[bad, selected(source, retries=1)], **TINY)
    assert any("git_sha" in finding.detail for finding in report.blocking)


def test_a_selected_attempt_with_an_altered_cost_is_refused() -> None:
    source = row()
    altered = selected(source) | {"dollars": 0.02}
    report = publication.gate([source], release=RELEASE, ledger=[altered], **TINY)
    assert any("dollars" in finding.detail for finding in report.blocking)


def test_an_attempt_with_a_negative_cost_is_refused() -> None:
    source = row()
    bad = selected(source) | {"dollars": -0.01}
    report = publication.gate([source], release=RELEASE, ledger=[bad], **TINY)
    assert any("finite non-negative" in finding.detail for finding in report.blocking)


@pytest.mark.parametrize(
    ("mutate", "check"),
    [
        (lambda r: r | {"unknown_cost": True}, "cost"),
        (lambda r: r | {"unknown_cost": False, "dollars": None}, "cost"),
        (lambda r: r | {"unmetered_requests": 1}, "cost"),
        (lambda r: r | {"passed": True, "correct": False}, "grade"),
        (lambda r: r | {"task_version": (TASK_VERSION or 0) + 1}, "version"),
        (lambda r: r | {"suite_version": "stale000"}, "version"),
        (lambda r: r | {"run": r["run"] | {"git_dirty": True}}, "build"),
        (lambda r: r | {"run": r["run"] | {"fastbrowse_version": "9.9.8"}}, "build"),
        (lambda r: r | {"run": r["run"] | {"arms": {}}}, "provenance"),
        (lambda r: r | {"run": r["run"] | {"max_steps": 0}}, "protocol"),
        (lambda r: r | {"run": r["run"] | {"providers": None}}, "provenance"),
    ],
)
def test_integrity_refuses_the_rule_it_breaks(mutate: Any, check: str) -> None:
    source = mutate(row())
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], require_ledger=True)
    assert check in [finding.check for finding in report.blocking]


@pytest.mark.parametrize("sha", ["HEAD", "main", "0123456789abcdef", "-n1"])
def test_a_non_commit_sha_is_refused(sha: str) -> None:
    """A ref name, an option or an abbreviation is not a build the release can be diffed against."""
    source = row(run=row()["run"] | {"git_sha": sha})
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], **TINY)
    assert any(finding.check == "build" and "40-hex" in finding.detail for finding in report.blocking)


def test_a_total_is_not_fabricated_when_cost_is_unknown() -> None:
    source = row(dollars=None, unknown_cost=True)
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], require_ledger=True)
    assert not any(finding.check == "cost" for finding in report.blocking)


def test_a_corpus_row_must_name_the_pinned_bytes() -> None:
    from fastbrowse.evals.sources import SOURCES

    windtunnel = SOURCES["windtunnel"]
    sources = [
        row(repeat=index)
        | {
            "corpus": "a" * 64,
            "source": "windtunnel",
            "revision": windtunnel.revision,
            "sha256": windtunnel.sha256,
            "task_digest": "b" * 64,
        }
        for index in range(3)
    ]
    assert publication.gate(sources, release=RELEASE, ledger=evidence(sources), **TINY).blocking == ()
    wrong = sources[0] | {"revision": "c" * 40}
    report = publication.gate([wrong], release=RELEASE, ledger=[selected(wrong)], **TINY)
    assert any(finding.check == "dataset" for finding in report.blocking)


def test_a_single_correctness_drop_is_blocked() -> None:
    baseline = [baseline_row(passed=True) for _ in range(3)]
    candidate = [*attempts(2), row(repeat=2, passed=False, correct=False)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert [finding.severity for finding in report.blocking] == [REGRESS]


def test_a_candidate_at_the_baseline_rate_is_not_a_regression() -> None:
    baseline = [baseline_row(passed=True) for _ in range(3)]
    candidate = attempts(3)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert report.blocking == ()


def _hosted(**overrides: Any) -> dict[str, Any]:
    arms = {
        "fastbrowse": {"pin": "run.fastbrowse_version + run.git_sha", "tier": "A"},
        "browser-use": {"pin": "browser-use-sdk==3.11.3", "tier": "hosted"},
    }
    return row(run=(row()["run"] | {"arms": arms}), **overrides)


def test_another_arms_failures_neither_block_nor_mask() -> None:
    baseline = [baseline_row(passed=True) for _ in range(3)]
    hosted_failures = [_hosted(arm="browser-use", passed=False, correct=False, repeat=index) for index in range(5)]
    clean = [*attempts(3), *hosted_failures]
    assert publication.gate(clean, release=RELEASE, ledger=evidence(clean), baselines_=baseline).blocking == ()
    hosted_passes = [_hosted(arm="browser-use", repeat=index) for index in range(5)]
    regressed = [row(repeat=index, passed=index < 1, correct=index < 1) for index in range(3)] + hosted_passes
    report = publication.gate(regressed, release=RELEASE, ledger=evidence(regressed), baselines_=baseline)
    assert any(finding.check == "regression" for finding in report.blocking)


def test_only_the_latest_matching_release_is_the_baseline() -> None:
    """An older high baseline must not hide a fall against the newest published one."""
    older = [baseline_row(passed=True, release="1.0.0") for _ in range(10)]
    latest = [baseline_row(passed=False, release="2.0.0") for _ in range(3)]
    candidate = [row(repeat=index, passed=index < 4, correct=index < 4) for index in range(10)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=[*older, *latest])
    assert report.blocking == ()


def test_a_comparison_behind_fewer_than_three_repeats_is_refused() -> None:
    baseline = [baseline_row(passed=True) for _ in range(3)]
    candidate = [*attempts(1), row(repeat=1, passed=False, correct=False)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert any("diagnostic" in finding.detail for finding in report.blocking)


def test_an_older_baseline_is_reported_but_never_blocks() -> None:
    stale = [baseline_row(passed=True, task_version=(TASK_VERSION or 0) - 1) for _ in range(10)]
    candidate = attempts(3)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=stale)
    assert report.blocking == ()
    assert any("only at v" in finding.detail for finding in report.findings if finding.check == "baseline")


def test_a_baseline_at_another_protocol_or_route_is_not_compared() -> None:
    baseline = [baseline_row(passed=True, providers="typesafe") for _ in range(3)]
    candidate = [row(repeat=index, passed=False, correct=False) for index in range(3)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert report.blocking == ()
    assert any("another protocol" in finding.detail for finding in report.findings if finding.check == "baseline")


def test_a_baseline_too_small_to_be_a_rate_is_reported_but_never_blocks() -> None:
    thin = [baseline_row(passed=True) for _ in range(2)]
    candidate = [*attempts(2), row(repeat=2, passed=False, correct=False)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=thin)
    assert report.blocking == ()
    assert any(finding.check == "baseline" for finding in report.findings)


def test_a_thin_newest_baseline_does_not_hide_an_older_full_one() -> None:
    older = [baseline_row(passed=True, release="1.0.0") for _ in range(3)]
    thin = [baseline_row(passed=False, release="2.0.0") for _ in range(2)]
    candidate = [row(repeat=index, passed=index < 1, correct=index < 1) for index in range(3)]
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=[*older, *thin])
    assert any(finding.check == "regression" for finding in report.blocking)


def test_material_latency_and_cost_increases_block() -> None:
    baseline = [baseline_row(passed=True, seconds=10.0, dollars=0.01) for _ in range(3)]
    candidate = attempts(3, seconds=13.0, dollars=0.02)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert {finding.check for finding in report.blocking} == {"speed", "cost"}


@pytest.mark.parametrize(("base_seconds", "candidate_seconds"), [(10.0, 10.5), (0.1, 0.2)])
def test_a_latency_rise_under_the_relative_bound_or_floor_does_not_block(
    base_seconds: float, candidate_seconds: float
) -> None:
    baseline = [baseline_row(passed=True, seconds=base_seconds) for _ in range(3)]
    candidate = attempts(3, seconds=candidate_seconds)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert not any(finding.check == "speed" for finding in report.blocking)


@pytest.mark.parametrize(("base_dollars", "candidate_dollars"), [(0.01, 0.0101), (0.0001, 0.0002)])
def test_a_cost_rise_under_the_relative_bound_or_floor_does_not_block(
    base_dollars: float, candidate_dollars: float
) -> None:
    baseline = [baseline_row(passed=True, dollars=base_dollars) for _ in range(3)]
    candidate = attempts(3, dollars=candidate_dollars)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert not any(finding.check == "cost" for finding in report.blocking)


def test_an_unpriced_candidate_never_compares_a_cost() -> None:
    baseline = [baseline_row(passed=True) for _ in range(5)]
    candidate = attempts(3, dollars=None, unknown_cost=True)
    report = publication.gate(candidate, release=RELEASE, ledger=evidence(candidate), baselines_=baseline)
    assert not any(finding.check == "cost" for finding in report.findings)


def test_the_cli_blocks_a_run_that_breaks_integrity(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    source = row()
    path = tmp_path / "rows.jsonl"
    path.write_text(json.dumps(source | {"passed": "false"}) + "\n", encoding="utf-8")
    assert publication.main(["--rows", str(path), "--baseline", str(tmp_path)]) == 1
    assert "BLOCKED" in capsys.readouterr().out


def test_the_cli_passes_a_clean_run(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    sources = attempts(3)
    path = tmp_path / "rows.jsonl"
    path.write_text("".join(json.dumps(source) + "\n" for source in sources), encoding="utf-8")
    publication.ledger_path(path).write_text(
        "".join(json.dumps(entry) + "\n" for entry in evidence(sources)), encoding="utf-8"
    )
    assert publication.main(["--rows", str(path), "--baseline", str(tmp_path), "--require-ledger"]) == 0
    assert "PASS" in capsys.readouterr().out


def test_the_cli_reports_a_missing_row_file(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    assert publication.main(["--rows", str(tmp_path / "missing.jsonl"), "--baseline", str(tmp_path)]) == 1
    assert "no eval rows" in capsys.readouterr().out


def test_publish_uses_the_same_gate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(versions, "RESULTS", tmp_path / "results")
    source = tmp_path / "rows.jsonl"
    source.write_text(json.dumps(row()) + "\n", encoding="utf-8")
    with pytest.raises(ValueError, match="ledger"):
        versions.publish(RELEASE, source)


def test_row_that_is_refused_is_absent_from_a_passed_report_ready_shape() -> None:
    source = row()
    report = publication.gate([source], release=RELEASE, ledger=[selected(source)], **TINY)
    assert REFUSE not in {finding.severity for finding in report.findings}


def _init_repo(path: Path) -> None:
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=path, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=path, check=True)


def _commit(path: Path, message: str) -> None:
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-q", "-m", message], cwd=path, check=True, capture_output=True)


def _published_change(tmp_path: Path, *, ledger: bool) -> Path:
    """A repo whose `work` branch adds a results file (and its ledger when asked) over `main`."""
    repo = tmp_path / "repo"
    (repo / "docs" / "results").mkdir(parents=True)
    _init_repo(repo)
    (repo / "README.md").write_text("base\n", encoding="utf-8")
    _commit(repo, "base")
    subprocess.run(["git", "checkout", "-q", "-b", "work"], cwd=repo, check=True, capture_output=True)
    sources = suite_rows()
    results = repo / "docs" / "results" / f"{RELEASE}.jsonl"
    results.write_text("".join(json.dumps(source) + "\n" for source in sources), encoding="utf-8")
    if ledger:
        publication.ledger_path(results).write_text(
            "".join(json.dumps(entry) + "\n" for entry in evidence(sources)), encoding="utf-8"
        )
    _commit(repo, "publish")
    return repo


def test_a_changed_result_file_without_a_ledger_fails_the_diff_check(tmp_path: Path) -> None:
    repo = _published_change(tmp_path, ledger=False)
    findings = publication.validate_diff("main", root=repo)
    assert any(finding.check == "ledger" for finding in findings)


def test_a_changed_result_file_with_its_ledger_clears_the_diff_check(tmp_path: Path) -> None:
    repo = _published_change(tmp_path, ledger=True)
    assert publication.validate_diff("main", root=repo) == []


def test_a_changed_ledger_history_fails_the_diff_check(tmp_path: Path) -> None:
    """A published attempt cannot be edited or dropped any more than a published row can."""
    repo = _published_base(tmp_path)
    ledger = repo / "docs" / "results" / f"{RELEASE}.attempts.jsonl"
    lines = ledger.read_text(encoding="utf-8").splitlines()
    ledger.write_text("\n".join(lines[:-1]) + "\n", encoding="utf-8")
    _commit(repo, "edit a published ledger")
    findings = publication.validate_diff("main", root=repo)
    assert any(finding.check == "history" and "ledger" in finding.detail for finding in findings)


def test_a_partial_live_suite_fails_the_diff_check(tmp_path: Path) -> None:
    """A `--only` subset reads as a whole core result, so the PR gate refuses it against the full baseline."""
    repo = tmp_path / "repo"
    (repo / "docs" / "results").mkdir(parents=True)
    _init_repo(repo)
    base = _at_release(suite_rows(), "9.9.8")
    base_results = repo / "docs" / "results" / "9.9.8.jsonl"
    base_results.write_text("".join(json.dumps(source) + "\n" for source in base), encoding="utf-8")
    publication.ledger_path(base_results).write_text(
        "".join(json.dumps(entry) + "\n" for entry in evidence(base)), encoding="utf-8"
    )
    (repo / "README.md").write_text("base\n", encoding="utf-8")
    _commit(repo, "publish the full baseline")
    subprocess.run(["git", "checkout", "-q", "-b", "work"], cwd=repo, check=True, capture_output=True)
    sources = attempts(3)
    results = repo / "docs" / "results" / f"{RELEASE}.jsonl"
    results.write_text("".join(json.dumps(source) + "\n" for source in sources), encoding="utf-8")
    publication.ledger_path(results).write_text(
        "".join(json.dumps(entry) + "\n" for entry in evidence(sources)), encoding="utf-8"
    )
    _commit(repo, "publish a subset")
    findings = publication.validate_diff("main", root=repo)
    assert any(finding.check == "coverage" for finding in findings)


def test_a_partial_live_suite_is_refused_without_any_baseline() -> None:
    """The build's task list is the measure, so the first publication of a suite cannot drop a current task."""
    sources = attempts(3)
    report = publication.gate(
        sources, release=RELEASE, ledger=evidence(sources), require_ledger=True, require_coverage=True
    )
    assert any(finding.check == "coverage" and finding.severity == REFUSE for finding in report.blocking)


def test_a_whole_current_suite_clears_coverage() -> None:
    sources = suite_rows()
    report = publication.gate(
        sources, release=RELEASE, ledger=evidence(sources), require_ledger=True, require_coverage=True
    )
    assert not any(finding.check == "coverage" for finding in report.blocking)


def test_core_coverage_ignores_an_unrelated_heldout_baseline() -> None:
    """A suite the candidate did not claim is not its business, however much a baseline ran of it."""
    sources = suite_rows("core")
    heldout = _at_release(suite_rows("heldout"), "9.9.8")
    report = publication.gate(
        sources, release=RELEASE, ledger=evidence(sources), baselines_=heldout, require_ledger=True
    )
    assert not any(finding.check == "coverage" for finding in report.blocking)
    assert report.blocking == ()


def _published_base(tmp_path: Path) -> Path:
    """A repo whose `main` already published a valid results file and its ledger."""
    repo = tmp_path / "repo"
    (repo / "docs" / "results").mkdir(parents=True)
    _init_repo(repo)
    sources = suite_rows()
    results = repo / "docs" / "results" / f"{RELEASE}.jsonl"
    results.write_text("".join(json.dumps(source) + "\n" for source in sources), encoding="utf-8")
    publication.ledger_path(results).write_text(
        "".join(json.dumps(entry) + "\n" for entry in evidence(sources)), encoding="utf-8"
    )
    _commit(repo, "publish")
    subprocess.run(["git", "checkout", "-q", "-b", "work"], cwd=repo, check=True, capture_output=True)
    return repo


def test_a_removed_result_file_fails_the_diff_check(tmp_path: Path) -> None:
    repo = _published_base(tmp_path)
    (repo / "docs" / "results" / f"{RELEASE}.jsonl").unlink()
    (repo / "docs" / "results" / f"{RELEASE}.attempts.jsonl").unlink()
    _commit(repo, "remove a baseline")
    findings = publication.validate_diff("main", root=repo)
    assert any("removed" in finding.detail for finding in findings)


@pytest.mark.parametrize("tamper", [False, True])
def test_migrated_baseline_must_preserve_original_scores(tmp_path: Path, tamper: bool) -> None:
    import hashlib

    from fastbrowse.evals import baseline

    repo = _published_base(tmp_path)
    results = repo / "docs/results" / f"{RELEASE}.jsonl"
    content = results.read_bytes()
    rows = baseline.scores([json.loads(line) for line in content.splitlines()])
    if tamper:
        rows[0]["passed"] = not rows[0]["passed"]
    entry = {
        "source": {"path": results.relative_to(repo).as_posix(), "sha256": hashlib.sha256(content).hexdigest()},
        "rows": rows,
    }
    (results.parent / baseline.NAME).write_text(json.dumps([entry]) + "\n")
    ledger = publication.ledger_path(results)
    ledger_content = ledger.read_bytes()
    (results.parent / "evidence.manifest.archive.json").write_text(
        json.dumps(
            [
                {
                    "path": results.relative_to(repo).as_posix(),
                    "sha256": hashlib.sha256(content).hexdigest(),
                    "bytes": len(content),
                    "observation_ids": ["c" * 16],
                    "trace_id": hashlib.sha256(content).hexdigest()[:32],
                    "stored_at": "2026-10-06T10:00:00Z",
                },
                {
                    "path": ledger.relative_to(repo).as_posix(),
                    "sha256": hashlib.sha256(ledger_content).hexdigest(),
                    "bytes": len(ledger_content),
                    "observation_ids": ["b" * 16],
                    "trace_id": hashlib.sha256(ledger_content).hexdigest()[:32],
                    "stored_at": "2026-10-06T10:00:00Z",
                },
            ]
        )
    )
    results.unlink()
    publication.ledger_path(results).unlink()
    _commit(repo, "migrate baseline")
    findings = publication.validate_diff("main", root=repo)
    assert any(finding.severity == REFUSE for finding in findings) is tamper
    assert publication.trusted_rows("HEAD", root=repo) == rows


def test_existing_compact_baseline_cannot_be_changed_or_removed(tmp_path: Path) -> None:
    from fastbrowse.evals import baseline

    repo = _published_base(tmp_path)
    results = repo / "docs/results" / f"{RELEASE}.jsonl"
    entry = {
        "source": {"path": results.relative_to(repo).as_posix(), "sha256": "a" * 64},
        "rows": baseline.scores([json.loads(line) for line in results.read_text().splitlines()]),
    }
    target = results.parent / baseline.NAME
    target.write_text(json.dumps([entry]) + "\n")
    _commit(repo, "compact base")
    subprocess.run(["git", "branch", "-f", "main", "HEAD"], cwd=repo, check=True, capture_output=True)
    target.write_text("[]\n")
    _commit(repo, "drop compact history")
    assert any("baseline changed" in finding.detail for finding in publication.validate_diff("main", root=repo))


def test_a_renamed_result_file_fails_the_diff_check(tmp_path: Path) -> None:
    repo = _published_base(tmp_path)
    old = repo / "docs" / "results" / f"{RELEASE}.jsonl"
    new = repo / "docs" / "results" / "9.9.90.jsonl"
    old.rename(new)
    publication.ledger_path(old).rename(publication.ledger_path(new))
    _commit(repo, "rename a baseline")
    findings = publication.validate_diff("main", root=repo)
    assert any("removed" in finding.detail for finding in findings)


def remaining_rows() -> list[dict[str, Any]]:
    sources = attempts(3)
    for index, source in enumerate(sources):
        source["run"].update(
            budget_policy="remaining-campaign-v1",
            max_steps=None,
            max_seconds=None,
            max_dollars=None,
            concurrency=1,
            budget_usd=10.0,
            browser_reserve_dollars=0.0,
            browser={"mode": "local-chrome"},
            agent_limits=Limits(max_dollars=10.0 - index * 0.01).model_dump(mode="json"),
        )
    return sources


def test_uncapped_remaining_funds_run_clears_publication_gate() -> None:
    sources = remaining_rows()
    assert publication.gate(sources, release=RELEASE, ledger=evidence(sources), **TINY).blocking == ()


@pytest.mark.parametrize(
    "field,value",
    [
        ("budget_policy", None),
        ("agent_limits", None),
        ("agent_limits", Limits(max_steps=50, max_dollars=10).model_dump(mode="json")),
        ("agent_limits", Limits(max_dollars=11).model_dump(mode="json")),
        ("agent_limits", Limits(max_dollars=float("inf")).model_dump(mode="json")),
        ("concurrency", 2),
        ("budget_usd", 0),
        ("browser", {"mode": "browser-use-cloud"}),
    ],
)
def test_uncapped_publication_refuses_unrecorded_or_inconsistent_policy(field: str, value: object) -> None:
    sources = remaining_rows()
    sources[0]["run"][field] = value
    report = publication.gate(sources, release=RELEASE, ledger=evidence(sources), **TINY)
    assert any(f.check == "protocol" for f in report.blocking)
