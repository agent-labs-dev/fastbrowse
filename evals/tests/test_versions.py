import contextlib
import dataclasses
import json
import subprocess
from pathlib import Path
from typing import Any

import pytest
from fastbrowse import build
from fastbrowse.evals import versions
from fastbrowse.evals.mock_tasks import TASKS as MOCK
from fastbrowse.evals.tasks import TASKS as LOCAL
from fastbrowse.models import Limits

from fastbrowse_evals import live, more_tasks
from fastbrowse_evals.live_tasks import LiveTask

ROOT = versions.ROOT
ALL = [*(t for suite in live.SUITES.values() for t in suite), *LOCAL, *MOCK]


def test_every_task_matches_its_recorded_version() -> None:
    # A failure names the command that settles it: `--bump TASK_ID --docs`.
    live_tasks = [t for suite in live.SUITES.values() for t in suite]
    assert [m for m in versions.mismatches(ALL) if m.split(":")[0] in {t.id for t in live_tasks}] == []


def test_fastbrowses_own_tasks_match_their_recorded_versions() -> None:
    assert versions.mismatches(ALL) == []


def test_a_changed_grader_changes_the_fingerprint() -> None:
    task = live.SUITES["core"][0]
    regraded = dataclasses.replace(task, check=lambda _outcome, _truth: None)
    assert versions.fingerprint(regraded) != versions.fingerprint(task)
    assert versions.fingerprint(dataclasses.replace(task)) == versions.fingerprint(task)


@pytest.mark.parametrize("task_id", ["google-flights", "flights-search"])
def test_a_rolling_date_keeps_the_version_from_one_day_to_the_next(task_id: str) -> None:
    task = next(t for suite in live.SUITES.values() for t in suite if t.id == task_id)
    [(today, name)] = task.rolling.items()
    assert today in task.task
    tomorrow = dataclasses.replace(
        task, task=task.task.replace(today, "1 January 2099"), rolling={"1 January 2099": name}
    )
    assert versions.fingerprint(tomorrow) == versions.fingerprint(task)
    assert versions.fingerprint(dataclasses.replace(tomorrow, task="Search for flights.")) != versions.fingerprint(task)


def test_closures_over_different_values_fingerprint_apart() -> None:
    def expecting(needle: str) -> LiveTask:
        return dataclasses.replace(live.SUITES["core"][0], check=lambda _outcome, _truth: needle)

    assert versions.fingerprint(expecting("a")) != versions.fingerprint(expecting("b"))


def test_a_shared_helper_is_part_of_every_grader_that_calls_it(monkeypatch: pytest.MonkeyPatch) -> None:
    # `_has` graders normalise through `_flat`; loosening it regrades them without touching their own source.
    task = next(t for t in more_tasks.DEV if t.id == "books-travel-priciest")
    before = versions.fingerprint(task)
    monkeypatch.setattr(more_tasks, "_flat", lambda text: text)
    assert versions.fingerprint(task) != before


def test_comments_and_layout_are_not_a_change() -> None:
    plain = "def check(outcome, truth):\n    return None\n"
    commented = "def check(outcome,   truth):  # why\n    # a note\n    return None\n"
    assert versions._tokens(plain) == versions._tokens(commented)
    assert versions._tokens(plain) != versions._tokens(plain.replace("None", "'x'"))


def test_a_suite_version_moves_with_any_task_version() -> None:
    lock = {"a": {"version": 1, "fingerprint": ""}, "b": {"version": 1, "fingerprint": ""}}
    before = versions.suite_version(["a", "b"], lock)
    assert versions.suite_version(["b", "a"], lock) == before
    assert versions.suite_version(["a", "b"], lock | {"b": {"version": 2, "fingerprint": ""}}) != before
    assert versions.suite_version(["a"], lock) != before


def test_only_rejects_an_id_the_selection_does_not_hold() -> None:
    with pytest.raises(SystemExit):
        live.asyncio.run(live.main(["--suite", "core", "--only", "hover-profile"]))
    with pytest.raises(SystemExit):
        live.asyncio.run(live.main(["--only", "no-such-task"]))


def test_provenance_counts_untracked_non_ignored_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A build with an untracked executable is not clean; an ignored artifact does not dirty it."""
    repo = tmp_path / "repo"
    (repo / "src").mkdir(parents=True)
    (repo / "src" / "agent.py").write_text("VALUE = 1\n", encoding="utf-8")
    (repo / ".gitignore").write_text("artifacts/\n", encoding="utf-8")

    def run(*args: str) -> None:
        subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)

    run("init", "-q", "-b", "main")
    run("config", "user.email", "test@example.com")
    run("config", "user.name", "Test")
    run("add", "-A")
    run("commit", "-q", "-m", "measured build")

    def git_in_repo(*args: str) -> str | None:
        done = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, timeout=10, check=False)
        return done.stdout.strip() if done.returncode == 0 else None

    monkeypatch.setattr(build, "_git", git_in_repo)
    assert versions.provenance()["git_dirty"] is False
    (repo / "artifacts").mkdir()
    (repo / "artifacts" / "nightly.jsonl").write_text("{}\n", encoding="utf-8")
    assert versions.provenance()["git_dirty"] is False, "an ignored artifact is not a dirty build"
    (repo / "src" / "sneaky.py").write_text("print('x')\n", encoding="utf-8")
    assert versions.provenance()["git_dirty"] is True, "an untracked source file is a dirty build"


def _row(task: str, *, arm: str = "fastbrowse", passed: bool = True, **run: Any) -> dict[str, Any]:
    return {
        "arm": arm,
        "task": task,
        "category": "lookup",
        "suite": "core",
        "suite_version": versions.suite_version(t.id for t in live.SUITES["core"]),
        "task_version": versions.task_version(task),
        "status": "complete",
        "passed": passed,
        "correct": passed,
        "seconds": 10.0,
        "dollars": 0.01,
        "retries": 0,
        "repeat": 0,
        "failure": None if passed else "answer lacks x",
        "model": "hosted-model-7",
        "trace": ["dropped from the published row"],
        "run": {
            "run_id": "r1",
            "run_started": "2026-09-25T10:00:00+00:00",
            "fastbrowse_version": "9.9.9",
            "git_sha": "0123456789abcdef" + "0" * 24,
            "git_dirty": False,
            "benchmark_catalog_sha256": __import__(
                "fastbrowse.evals.catalog", fromlist=["CATALOG_SHA256"]
            ).CATALOG_SHA256,
            "benchmark_runner_sha": "1" * 40,
            "benchmark_family": "browser-use",
            "benchmark_origin": "fastbrowse-internal",
            "benchmark_runner_dirty": False,
            "providers": "openrouter",
            # The uncapped start-gate protocol: the budget only decided whether a run started.
            "budget_policy": "start-gate-v1",
            "budget_usd": 100.0,
            "max_steps": None,
            "max_seconds": None,
            "max_dollars": None,
            "agent_limits": Limits().model_dump(mode="json"),
            "browsers": {
                "fastbrowse": {"mode": "browser-use-cloud", "timeout_minutes": 240},
                "browser-use": {"mode": "browser-use-hosted"},
            },
            "concurrency": 2,
            "arms": {
                "fastbrowse": {"pin": "run.fastbrowse_version + run.git_sha", "tier": "A"},
                "browser-use": {"pin": "browser-use-sdk==3.11.3", "tier": "hosted"},
            },
            "argv": ["--suite", "core"],
        }
        | run,
    }


def _repeats(task: str, *, count: int = 3, **kwargs: Any) -> list[dict[str, Any]]:
    """`count` distinct attempts of one task, so a published comparison clears the three-repeat rule."""
    return [_row(task, **kwargs) | {"repeat": index} for index in range(count)]


def _attempt(row: dict[str, Any]) -> dict[str, Any]:
    """The one selected attempt `_row` reports, as the ledger beside it would record it."""
    return {
        "selected": True,
        "retry_wait_seconds": 0,
        "arm": row["arm"],
        "task": row["task"],
        "repeat": row["repeat"],
        "retries": row["retries"],
        "status": row["status"],
        "normalized_status": row.get("normalized_status"),
        "seconds": row["seconds"],
        "dollars": row["dollars"],
        "run": row["run"],
    }


@pytest.fixture
def results(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr(versions, "RESULTS", tmp_path / "results")
    return tmp_path


@contextlib.contextmanager
def _publishing(rows: list[dict[str, Any]]):
    """Narrow the build's `core` suite to the fixture tasks `rows` carry, at their canonical version.

    A published live suite is now held to every task this build defines. These unit fixtures are one or two
    shapes, not a live suite, so the build they publish under defines `core` as exactly their tasks. The
    production gate is not relaxed: it still refuses a candidate that omits a task the build it is handed
    defines, which the focused coverage tests exercise against the real suites.
    """
    fixture = tuple(task for task in live.SUITES["core"] if task.id in {str(item["task"]) for item in rows})
    version = versions.suite_version(task.id for task in fixture)
    for item in rows:
        if item.get("suite") == "core":
            item["suite_version"] = version
    with pytest.MonkeyPatch.context() as patch:
        patch.setattr(live, "SUITES", {**live.SUITES, "core": fixture})
        from fastbrowse.evals import catalog

        patch.setattr(catalog, "SUITES", {**catalog.SUITES, "core": fixture})
        yield


def _write(results: Path, rows: list[dict[str, Any]], name: str) -> Path:
    source = results / name
    source.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    source.with_suffix(".attempts.jsonl").write_text(
        "".join(json.dumps(_attempt(r)) + "\n" for r in rows), encoding="utf-8"
    )
    return source


def _publish(results: Path, rows: list[dict[str, Any]]) -> Path:
    with _publishing(rows):
        return versions.publish("9.9.9", _write(results, rows, "rows.jsonl"))


def test_publishing_refuses_rows_that_cannot_be_traced_or_compared(results: Path) -> None:
    for bad in (
        _row("pypi-version", git_dirty=True),
        _row("pypi-version", git_sha=None),
        _row("pypi-version", fastbrowse_version="9.9.8"),
        _row("pypi-version") | {"task_version": 0},
        _row("pypi-version") | {"seconds": float("nan")},
        _row("pypi-version") | {"dollars": -1},
        _row("pypi-version") | {"passed": "false"},
        # A task missing from the lock has no version to compare, so its null version must not match.
        _row("no-such-task"),
    ):
        with pytest.raises(ValueError):
            _publish(results, [bad])
    assert not (results / "results").exists()


def test_published_rows_are_slim_immutable_and_keep_baseline_identity(results: Path) -> None:
    rival = [*_repeats("pypi-version", arm="browser-use"), *_repeats("hn-top", arm="browser-use")]
    published = _publish(results, [*_repeats("pypi-version"), *_repeats("hn-top", passed=False), *rival])
    assert "trace" not in published.read_text(encoding="utf-8")
    # The model and invocation are provenance: a hosted arm's score means nothing without the model behind it.
    kept = json.loads(published.read_text(encoding="utf-8").splitlines()[0])
    assert kept["model"] == "hosted-model-7" and kept["run"]["argv"] == ["--suite", "core"]
    # Arms are compared attempt by attempt: without its repeat, a row pairs with whichever came first.
    assert kept["repeat"] == 0
    with pytest.raises(ValueError, match="never rewritten"):
        _publish(results, [_row("pypi-version")])
    from fastbrowse.evals import baseline, publication

    compact = baseline.Baseline(
        source={"path": "docs/results/9.9.9.jsonl", "sha256": "a" * 64},
        rows=baseline.scores([json.loads(line) for line in published.read_text().splitlines()]),
    )
    (published.parent / baseline.NAME).write_text("[" + compact.model_dump_json() + "]")
    published.unlink()
    publication.ledger_path(published).unlink()
    with pytest.raises(ValueError, match="never rewritten"):
        _publish(results, _repeats("pypi-version"))


def test_publication_keeps_rows_and_attempts_without_rewriting_prose(results: Path) -> None:
    target = _publish(results, _repeats("pypi-version"))
    assert len(target.read_text().splitlines()) == 3
    assert target.with_suffix(".attempts.jsonl").exists()


def _source(results: Path, rows: list[dict[str, Any]], name: str) -> Path:
    return _write(results, rows, name)


def test_release_sections_stay_newest_first_however_they_are_added(results: Path) -> None:
    for release in ("9.10.0", "9.9.9"):
        rows = _repeats("pypi-version", fastbrowse_version=release)
        with _publishing(rows):
            versions.publish(release, _source(results, rows, f"{release}.rows"))
    assert [release for release, _ in versions.published()] == ["9.10.0", "9.9.9"]


def test_summary_groups_suites_and_preserves_unknown_costs() -> None:
    first = _row("pypi-version", fastbrowse_version="1.0.0") | {"task_version": 1}
    later = _row("pypi-version", fastbrowse_version="1.0.1") | {"task_version": 2, "suite_version": "changed"}
    second = later | {"seconds": 30.0, "dollars": None, "passed": False}
    other = later | {"suite": "dev", "suite_version": "dev-version", "arm": "browser-use"}
    feed = versions.summary([("1.0.1", [later, second, other]), ("1.0.0", [first])])
    assert feed.schema_version == 2 and len(feed.releases) == 3
    core = next(r for r in feed.releases if r.fastbrowse_version == "1.0.1" and r.suite == "core")
    arm = core.arms["fastbrowse"]
    assert (arm.passed, arm.total, arm.priced) == (1, 2, 1)
    assert arm.seconds.mean == arm.seconds.median == 20.0
    assert arm.dollars.mean is arm.dollars.median is None
    assert core.task_versions_changed == [versions.TaskChange(task="pypi-version", previous=[1], current=[2])]
    old = next(r for r in feed.releases if r.fastbrowse_version == "1.0.0")
    assert not old.task_versions_changed
    assert old.arms["fastbrowse"].dollars.mean == 0.01


def test_the_feed_leads_each_release_with_the_core_suite() -> None:
    """The site headlines the feed's first entry; stretch-heldout sorting last by name put 3 hard tasks there."""
    rows = [_row("pypi-version") | {"suite": suite} for suite in ("stretch-heldout", "dev", "core", "stretch-dev")]
    feed = versions.summary([("1.0.0", [_row("pypi-version") | {"suite": "stretch-heldout"}]), ("1.0.1", rows)])
    order = [(r.fastbrowse_version, r.suite) for r in feed.releases]
    assert order == [
        ("1.0.1", "core"),
        ("1.0.1", "dev"),
        ("1.0.1", "stretch-dev"),
        ("1.0.1", "stretch-heldout"),
        ("1.0.0", "stretch-heldout"),
    ]


def test_an_outage_scores_no_arm_and_takes_a_matching_attempt_from_each() -> None:
    """0.5.6 dropped fastbrowse's outages alone, leaving it on 13 core tasks beside Browser Use on 14. Failed
    requests measured inside an attempt stay in its time: only fastbrowse's could be seen, so subtracting them
    credited it alone."""
    waited = _row("pypi-version") | {"seconds": 40.0, "transient_seconds": 30.0, "at": 1.0}
    outage = _row("pypi-version") | {"normalized_status": "unavailable", "passed": False, "seconds": 300.0, "repeat": 1}
    first = _row("pypi-version", arm="browser-use") | {"at": 2.0}
    # Earlier than its pair, it still pairs with fastbrowse's lost repeat, not the one fastbrowse measured.
    later = _row("pypi-version", arm="browser-use", passed=False) | {"at": 0.5, "repeat": 1}
    lost = [_row("hn-top") | {"normalized_status": "unavailable"}, _row("hn-top", arm="browser-use")]
    rows = [waited, outage, first, later, *lost]
    arms = versions.summary([("1.0.0", rows)]).releases[0]
    assert arms.tasks == 1
    assert [(a.passed, a.total, a.excluded) for a in arms.arms.values()] == [(1, 1, 2), (1, 1, 2)]
    assert arms.arms["fastbrowse"].seconds.median == 40.0


def test_an_outage_drops_the_same_repeat_from_every_arm() -> None:
    """Paired earliest first, fastbrowse's second attempt was scored beside Browser Use's first when fastbrowse's
    first was lost to an outage."""
    fast = [
        _row("pypi-version") | {"normalized_status": "unavailable", "repeat": 0},
        _row("pypi-version", passed=False) | {"repeat": 1},
    ]
    hosted = [
        _row("pypi-version", arm="browser-use") | {"repeat": 0, "at": 1.0},
        _row("pypi-version", arm="browser-use", passed=False) | {"repeat": 1, "at": 2.0},
    ]
    arms = versions.summary([("1.0.0", [*fast, *hosted])]).releases[0].arms
    assert [(a.passed, a.total) for a in arms.values()] == [(0, 1), (0, 1)]


def test_separate_outage_retry_keeps_original_pair_and_actual_run_provenance() -> None:
    fast = _row("pypi-version")
    retry = _row("pypi-version", arm="browser-use")
    retry["run"] = dict(retry["run"], run_id="retry-run")
    retry["replaces_run_id"] = "r1"
    published = versions.slim(retry)
    arms = versions.summary([("1.0.0", [fast, published])]).releases[0].arms
    assert [(a.passed, a.total) for a in arms.values()] == [(1, 1), (1, 1)]
    assert published["run"]["run_id"] == "retry-run"


def test_every_comparison_sets_arms_on_the_same_tasks() -> None:
    """Pooled, core set fastbrowse beside Browser Use on some tasks and Browser Harness on others."""
    rows = [
        _row("saucedemo-cart"),
        _row("saucedemo-cart", arm="browser-use"),
        _row("hn-comments"),
        _row("hn-comments", arm="browser-harness"),
        _row("wiki-open", passed=False),
    ]
    feed = versions.summary([("1.0.0", rows)])
    assert [(r.compared, r.tasks, r.arms["fastbrowse"].total) for r in feed.releases] == [
        # browser-harness cannot run the login task, so it is compared there too, failing it.
        (["fastbrowse", "browser-use", "browser-harness"], 1, 1),
        (["fastbrowse", "browser-harness"], 1, 1),
        (["fastbrowse"], 1, 1),
    ]


def test_summary_does_not_mix_suite_versions_or_invent_legacy_rows() -> None:
    assert versions.summary([]).model_dump() == {"schema_version": 2, "releases": []}
    row = _row("pypi-version")
    feed = versions.summary([("9.9.9", [row, row | {"suite_version": "other", "seconds": 20}])])
    assert len(feed.releases) == 2
    assert {r.arms["fastbrowse"].seconds.mean for r in feed.releases} == {10, 20}


@pytest.mark.parametrize("current", [1, 2])
def test_summary_remembers_live_versions_across_mock_only_releases(current: int) -> None:
    live_row = _row("pypi-version", fastbrowse_version="1.0.0") | {"task_version": 1}
    mock_row = _row("mock-stock-count", fastbrowse_version="1.0.1") | {"suite": "mock-completion"}
    latest = live_row | {"task_version": current}
    report = versions.summary([("1.0.0", [live_row]), ("1.0.1", [mock_row]), ("1.0.2", [latest])])
    expected = [] if current == 1 else [versions.TaskChange(task="pypi-version", previous=[1], current=[2])]
    assert report.releases[0].task_versions_changed == expected


def test_publish_auto_uses_recorded_release(results: Path) -> None:
    rows = _repeats("pypi-version")
    with _publishing(rows):
        source = _source(results, rows, "rows.jsonl")
        assert versions.publish("auto", source).name == "9.9.9.jsonl"
        with pytest.raises(ValueError, match="numeric"):
            versions.publish("../escape", source)


def test_status_rule_is_versioned(monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse.evals import status

    before = versions.fingerprint(live.SUITES["core"][0])
    monkeypatch.setattr(status, "status_matches", lambda *_: True)
    assert versions.fingerprint(live.SUITES["core"][0]) != before


def test_live_grading_rule_is_versioned(monkeypatch: pytest.MonkeyPatch) -> None:
    before = versions.fingerprint(live.SUITES["core"][0])
    from fastbrowse.evals import grade as grading

    monkeypatch.setattr(grading, "grade", lambda *_: (True, None, None), raising=False)
    assert versions.fingerprint(live.SUITES["core"][0]) != before


def test_live_grading_does_not_change_mock_versions(monkeypatch: pytest.MonkeyPatch) -> None:
    before = [versions.fingerprint(task) for task in MOCK]
    from fastbrowse.evals import grade as grading

    monkeypatch.setattr(grading, "grade", lambda *_: (True, None, None))
    assert [versions.fingerprint(task) for task in MOCK] == before


@pytest.mark.parametrize("document", [None, {"status": 419, "title": "Error", "text_length": 10}])
def test_publish_rejects_navigation_pass_without_a_successful_document(results: Path, document: object) -> None:
    row = _row("pypi-open") | {"category": "navigate", "final_page": document}
    with pytest.raises(ValueError):
        _publish(results, [row])
    assert not (results / "results").exists()


@pytest.mark.parametrize("passed", [True, False])
@pytest.mark.parametrize("width", [None, 1520])
def test_publish_rejects_navigation_with_missing_or_mismatched_viewport(
    results: Path, passed: bool, width: int | None
) -> None:
    row = _row("pypi-open", passed=passed) | {
        "category": "navigate",
        "final_page": {
            "status": 200,
            "title": "httpx",
            "text_length": 40,
            "inner_width": width,
            "inner_height": 780,
            "device_pixel_ratio": 1,
        },
    }
    with pytest.raises(ValueError, match="navigation viewport"):
        _publish(results, [row])
    assert not (results / "results").exists()


@pytest.mark.parametrize("scale", [1.0000000149011612, 2.0])
def test_navigation_publication_tolerates_only_viewport_scale_rounding(results: Path, scale: float) -> None:
    row = _row("pypi-open") | {
        "category": "navigate",
        "final_page": {
            "status": 200,
            "title": "httpx",
            "text_length": 40,
            "inner_width": 1120,
            "inner_height": 780,
            "device_pixel_ratio": scale,
        },
    }
    if scale < 1.000001:
        assert _publish(results, [row | {"repeat": index} for index in range(3)]).exists()
    else:
        with pytest.raises(ValueError, match="navigation viewport"):
            _publish(results, [row])


def test_publication_keeps_the_attempt_ledger_out_of_score_aggregates(results: Path) -> None:
    rows = _repeats("pypi-version")
    with _publishing(rows):
        source = _write(results, rows, "rows.jsonl")
        attempts = [_attempt(row) | {"trace": ["a public page quote"]} for row in rows]
        source.with_suffix(".attempts.jsonl").write_text("".join(json.dumps(row) + "\n" for row in attempts))
        target = versions.publish("9.9.9", source)
    ledger = target.with_suffix(".attempts.jsonl")
    assert ledger.exists()
    assert [json.loads(line) for line in ledger.read_text().splitlines()] == attempts
    assert [release for release, _ in versions.published()] == ["9.9.9"]
    assert json.loads(versions.render_summary())["releases"][0]["fastbrowse_version"] == "9.9.9"


def test_publication_never_overwrites_an_existing_attempt_ledger(results: Path) -> None:
    folder = results / "results"
    folder.mkdir()
    ledger = folder / "9.9.9.attempts.jsonl"
    ledger.write_text("retained evidence\n")
    with pytest.raises(ValueError, match="never rewritten"):
        _publish(results, _repeats("pypi-version"))
    assert ledger.read_text() == "retained evidence\n"
    assert not (folder / "9.9.9.jsonl").exists()


def test_a_failed_publication_write_removes_only_its_own_files(results: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    original = Path.open

    def fail_scored(path: Path, *args: Any, **kwargs: Any):
        if path.name == "9.9.9.jsonl" and args == ("x",):
            raise OSError("disk full")
        return original(path, *args, **kwargs)

    with monkeypatch.context() as patch:
        patch.setattr(Path, "open", fail_scored)
        with pytest.raises(OSError, match="disk full"):
            _publish(results, _repeats("pypi-version"))
    assert not (results / "results" / "9.9.9.attempts.jsonl").exists()
    assert not (results / "results" / "9.9.9.jsonl").exists()
    assert _publish(results, _repeats("pypi-version")).exists()


def test_a_task_an_arm_cannot_run_is_a_failure_at_every_attempt() -> None:
    """The catalog says Browser Use cannot run saucedemo-pause, so the head-to-head scores it there as three failed
    attempts beside fastbrowse's three; time and cost are averaged over the attempts that ran."""
    from fastbrowse.evals.catalog import SUITES

    pause = next(t for s in SUITES.values() for t in s if t.id == "saucedemo-pause")
    assert "browser-use" in pause.excluded
    paused = [_row("saucedemo-pause") | {"task_version": pause.task_version, "repeat": r} for r in range(3)]
    both = [_row("pypi-version", arm=arm) | {"repeat": r} for arm in ("fastbrowse", "browser-use") for r in range(3)]
    paired = versions._paired({"fastbrowse": paused}, {"browser-use": "cannot"})
    assert [(r["arm"], r["repeat"], r["passed"]) for r in paired[3:]] == [("browser-use", r, False) for r in range(3)]
    [entry] = versions.summary([("1.0.0", [*paused, *both])]).releases
    assert entry.compared == ["fastbrowse", "browser-use"] and entry.tasks == 2
    rival = entry.arms["browser-use"]
    assert (rival.passed, rival.total, rival.unsupported, rival.priced) == (3, 6, 3, 3)
    assert rival.seconds.mean == 10.0
