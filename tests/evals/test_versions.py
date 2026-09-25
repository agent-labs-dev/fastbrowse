import dataclasses
import json
import re
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.evals import live, more_tasks, versions
from fastbrowse.evals.live_tasks import LiveTask
from fastbrowse.evals.tasks import TASKS as LOCAL

ROOT = Path(__file__).parents[2]
DOCS = (ROOT / "docs" / "evals.md").read_text(encoding="utf-8")
README = (ROOT / "README.md").read_text(encoding="utf-8")
ALL = [*(t for suite in live.SUITES.values() for t in suite), *LOCAL]


def test_every_task_matches_its_recorded_version() -> None:
    # A failure names the command that settles it: `--bump TASK_ID --docs`.
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


def _row(task: str, *, arm: str = "fastbrowse", passed: bool = True, **run: Any) -> dict[str, Any]:
    return {
        "arm": arm,
        "task": task,
        "category": "lookup",
        "suite": "core",
        "suite_version": "abcd1234",
        "task_version": versions.task_version(task),
        "status": "complete",
        "passed": passed,
        "correct": passed,
        "seconds": 10.0,
        "dollars": 0.01,
        "retries": 0,
        "failure": None if passed else "answer lacks x",
        "model": "hosted-model-7",
        "trace": ["dropped from the published row"],
        "run": {
            "run_id": "r1",
            "run_started": "2026-09-25T10:00:00+00:00",
            "fastbrowse_version": "9.9.9",
            "git_sha": "0123456789",
            "git_dirty": False,
            "argv": ["--suite", "core"],
        }
        | run,
    }


@pytest.fixture
def results(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr(versions, "RESULTS", tmp_path / "results")
    return tmp_path


def _publish(results: Path, rows: list[dict[str, Any]]) -> Path:
    source = results / "rows.jsonl"
    source.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    return versions.publish("9.9.9", source)


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


def test_published_rows_are_slim_immutable_and_generate_the_tables(results: Path) -> None:
    published = _publish(results, [_row("pypi-version"), _row("hn-top", passed=False)])
    assert "trace" not in published.read_text(encoding="utf-8")
    # The model and invocation are provenance: a hosted arm's score means nothing without the model behind it.
    kept = json.loads(published.read_text(encoding="utf-8").splitlines()[0])
    assert kept["model"] == "hosted-model-7" and kept["run"]["argv"] == ["--suite", "core"]
    with pytest.raises(ValueError, match="never rewritten"):
        _publish(results, [_row("pypi-version")])
    table = versions.docs_blocks()["results:9.9.9"]
    assert "| fastbrowse (9.9.9) | 1/2 | 1/2 | 10.0s | 10.0s | $0.0100 | $0.0100 | $0.02 |" in table
    assert "`r1` at `0123456`" in table
    readme = "x\n<!-- evals:headline -->\nstale\n<!-- /evals:headline -->\n"
    assert "| fastbrowse | 1/2 | $0.0100 (median), $0.0100 mean | 10.0s |" in versions.render_readme(readme)
    with pytest.raises(ValueError, match="no <!-- evals:headline -->"):
        versions.render_readme("no markers")


def test_a_first_publication_writes_its_own_section_or_nothing(results: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    bare = results / "README.md"
    bare.write_text("no markers\n", encoding="utf-8")
    monkeypatch.setattr(versions, "README", bare)
    with pytest.raises(ValueError, match="evals:headline"):
        _publish(results, [_row("pypi-version")])
    assert not (results / "results").exists()
    monkeypatch.undo()
    monkeypatch.setattr(versions, "RESULTS", results / "results")
    _publish(results, [_row("pypi-version")])
    docs = versions.render_docs(DOCS)
    assert docs.index("### 9.9.9, 2026-09-25") < docs.index("### 0.5.2")
    assert "| fastbrowse (9.9.9) | 1/1 |" in docs
    assert versions.render_docs(docs) == docs


def test_release_sections_stay_newest_first_however_they_are_added(results: Path) -> None:
    for release in ("9.10.0", "9.9.9"):
        source = results / f"{release}.rows"
        source.write_text(json.dumps(_row("pypi-version", fastbrowse_version=release)) + "\n", encoding="utf-8")
        versions.publish(release, source)
    docs = versions.render_docs(DOCS)
    assert docs.index("### 9.10.0,") < docs.index("### 9.9.9,") < docs.index("### 0.5.2,")


def test_a_published_table_names_tasks_changed_since(results: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _publish(results, [_row("pypi-version")])
    lock = versions.load_lock()
    previous = lock["pypi-version"]["version"]
    monkeypatch.setattr(
        versions, "load_lock", lambda: lock | {"pypi-version": {"version": previous + 1, "fingerprint": ""}}
    )
    assert f"`pypi-version` v{previous} → v{previous + 1}" in versions.docs_blocks()["results:9.9.9"]


def test_generated_docs_are_current() -> None:
    # Run `python -m fastbrowse.evals.versions --docs` after changing a task or publishing results.
    assert versions.render_docs(DOCS) == DOCS
    assert versions.render_readme(README) == README


def _section(text: str, heading: str) -> str:
    start = text.index(heading)
    end = text.find("\n## ", start + len(heading))
    return text[start : None if end == -1 else end]


def _rows(section: str) -> list[list[str]]:
    table = [line.strip("|").split("|") for line in section.splitlines() if line.startswith("| ")]
    return [[cell.strip() for cell in row] for row in table[1:]]


def _ids(cell: str) -> set[str]:
    return set(re.findall(r"`([a-z0-9-]+)`", cell))


def test_the_documented_core_and_local_tasks_are_the_defined_ones() -> None:
    core = {t.id: t.category.value for t in live.SUITES["core"]}
    documented = {task: row[0] for row in _rows(_section(DOCS, "## Live head-to-head")) for task in _ids(row[1])}
    assert documented == core
    assert {i for row in _rows(_section(DOCS, "## Local fixtures")) for i in _ids(row[0])} == {t.id for t in LOCAL}


def test_summary_groups_suites_and_preserves_unknown_costs() -> None:
    first = _row("pypi-version", fastbrowse_version="1.0.0") | {"task_version": 1}
    later = _row("pypi-version", fastbrowse_version="1.0.1") | {"task_version": 2, "suite_version": "changed"}
    second = later | {"seconds": 30.0, "dollars": None, "passed": False}
    other = later | {"suite": "dev", "suite_version": "dev-version", "arm": "browser-use"}
    feed = versions.summary([("1.0.1", [later, second, other]), ("1.0.0", [first])])
    assert feed.schema_version == 1 and len(feed.releases) == 3
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


def test_provider_outages_count_in_no_figure() -> None:
    """An outage says nothing about the agent: 0.5.6 read 55/63 at 28.6s on core, 55/56 at 20.6s without them."""
    waited = _row("pypi-version") | {"seconds": 40.0, "transient_seconds": 30.0}
    outage = _row("pypi-version") | {"normalized_status": "unavailable", "passed": False, "seconds": 300.0}
    feed = versions.summary([("1.0.0", [waited, outage])])
    arm = feed.releases[0].arms["fastbrowse"]
    assert (arm.passed, arm.total, arm.excluded) == (1, 1, 1)
    assert arm.seconds.median == 10.0 and arm.dollars.mean == 0.01
    rows = [waited, outage]
    assert "Excluded as provider outages: fastbrowse 1." in versions.headline("1.0.0", rows)
    only = versions.summary([("1.0.0", [outage])]).releases[0].arms["fastbrowse"]
    assert (only.total, only.excluded, only.seconds.median) == (0, 1, None)


def test_summary_does_not_mix_suite_versions_or_invent_legacy_rows() -> None:
    assert versions.summary([]).model_dump() == {"schema_version": 1, "releases": []}
    row = _row("pypi-version")
    feed = versions.summary([("9.9.9", [row, row | {"suite_version": "other", "seconds": 20}])])
    assert len(feed.releases) == 2
    assert {r.arms["fastbrowse"].seconds.mean for r in feed.releases} == {10, 20}


def test_markdown_keeps_suite_versions_separate_and_unpriced_cost_unknown() -> None:
    row = _row("pypi-version")
    rows = [row, row | {"suite_version": "other", "seconds": 20, "dollars": None}]
    for text in (versions.results_table("9.9.9", rows), versions.headline("9.9.9", rows)):
        assert "abcd1234" in text and "other" in text
        assert "10.0s" in text and "20.0s" in text and "15.0s" not in text
        assert "unknown" in text
    mixed = versions._arm_stats([row, row | {"dollars": None}])
    assert mixed["median cost"] == mixed["mean cost"] == "unknown"


def test_legacy_aggregates_generate_both_docs_without_inventing_rows() -> None:
    old = json.loads(versions.LEGACY.read_text())
    assert sum(a["total"] for a in old["arms"].values()) == old["tasks"] * old["repeats"] * len(old["arms"])
    for arm in old["arms"].values():
        for text in (versions.legacy_docs(), versions.legacy_docs(headline_only=True)):
            assert f"{arm['passed']}/{arm['total']}" in text
            assert f"${arm['median_dollars']:.4f}" in text
    assert "30-step" in versions.legacy_docs() and "historical" in versions.legacy_docs()


def test_committed_summary_is_generated_from_published_rows() -> None:
    assert (versions.RESULTS / "summary.json").read_text() == versions.render_summary()


def test_publish_auto_uses_recorded_release(results: Path) -> None:
    source = results / "rows.jsonl"
    source.write_text(json.dumps(_row("pypi-version")) + "\n")
    assert versions.publish("auto", source).name == "9.9.9.jsonl"
    with pytest.raises(ValueError, match="numeric"):
        versions.publish("../escape", source)


def test_status_rule_is_versioned(monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse.evals import status

    before = versions.fingerprint(live.SUITES["core"][0])
    monkeypatch.setattr(status, "status_matches", lambda *_: True)
    assert versions.fingerprint(live.SUITES["core"][0]) != before
