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
        "trace": ["dropped from the published row"],
        "run": {
            "run_id": "r1",
            "run_started": "2026-09-25T10:00:00+00:00",
            "fastbrowse_version": "9.9.9",
            "git_sha": "0123456789",
            "git_dirty": False,
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
    ):
        with pytest.raises(ValueError):
            _publish(results, [bad])
    assert not (results / "results").exists()


def test_published_rows_are_slim_immutable_and_generate_the_tables(results: Path) -> None:
    published = _publish(results, [_row("pypi-version"), _row("hn-top", passed=False)])
    assert "trace" not in published.read_text(encoding="utf-8")
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
    monkeypatch.setattr(versions, "load_lock", lambda: lock | {"pypi-version": {"version": 2, "fingerprint": ""}})
    assert "`pypi-version` v1 → v2" in versions.docs_blocks()["results:9.9.9"]


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


def test_the_readme_headline_is_the_latest_published_result() -> None:
    # Before the first committed release the headline is prose; afterwards test_generated_docs_are_current owns it.
    if versions.published():
        return
    heading = re.search(r"^### (\S+), (\d{4}-\d{2}-\d{2})$", DOCS, re.M)
    assert heading is not None
    release, day = heading.groups()
    assert f"Measured on {day} with the build released as {release}" in README
    rows = {}
    for cells in _rows(DOCS[heading.end() :]):
        if len(cells) == 8:
            rows.setdefault(re.sub(r" \(.*\)$", "", cells[0]), cells)
    for arm, headline in (("fastbrowse", "**fastbrowse**"), ("Browser Use agent", "Browser Use agent")):
        _, passed, _, median_time, _, median_cost, mean_cost, _ = rows[arm]
        line = next(line for line in README.splitlines() if line.startswith(f"| {headline} |"))
        plain = line.replace("**", "")
        assert f"| {passed} |" in plain
        assert f"{median_cost} (median), {mean_cost} mean" in plain
        assert f"| {median_time} |" in plain
