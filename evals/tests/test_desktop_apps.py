"""The desktop apps eval's grading, with no app, compositor or model."""

from pathlib import Path

import pytest

from fastbrowse_evals.desktop.apps import TASKS, AppTask, holds, missing, outcome, posted, serve

BY_ID = {task.id: task for task in TASKS}


def test_task_ids_are_unique_and_a_claim_is_only_for_a_task_that_reads() -> None:
    assert len(BY_ID) == len(TASKS)
    assert all(not (task.claim and task.impossible) for task in TASKS)


@pytest.mark.parametrize(
    ("impossible", "status", "fault", "want"),
    [
        (False, "complete", "", "pass"),
        (False, "stuck", "", "unreported"),
        (False, "stuck", "not done", "fail"),
        (False, "complete", "not done", "false complete"),
        (True, "stuck", "", "pass"),
        (True, "stuck", "the file changed", "fail"),
        (True, "complete", "", "false complete"),
    ],
)
def test_a_run_reported_complete_without_the_work_is_a_false_complete(
    impossible: bool, status: str, fault: str, want: str
) -> None:
    task = AppTask(
        id="t", kind="k", task="x", setup=lambda work, port: [], fault=lambda work: "", impossible=impossible
    )
    assert outcome(task, status, fault) == want


def test_a_file_is_compared_without_its_trailing_newlines(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("one\ntwo")
    assert holds(tmp_path / "a.txt", "one\ntwo\n") == ""
    assert holds(tmp_path / "a.txt", "one\n") != ""
    assert holds(tmp_path / "absent.txt", "") != ""


def test_each_file_task_is_not_done_before_the_agent_acts(tmp_path: Path) -> None:
    # A fault that is already "" on the fixtures would pass a run that did nothing.
    for name in ("editor-type-save", "editor-save-as", "editor-replace", "editor-tabs", "files-rename", "files-drag"):
        work = tmp_path / name
        BY_ID[name].setup(work, 0)
        assert BY_ID[name].fault(work), name


def test_an_impossible_task_starts_unchanged(tmp_path: Path) -> None:
    task = BY_ID["editor-impossible"]
    task.setup(tmp_path, 0)
    assert task.fault(tmp_path) == ""
    (tmp_path / "notes.txt").write_text("| a | b | c |\n")
    assert task.fault(tmp_path)


def test_a_page_post_is_kept_for_grading(tmp_path: Path) -> None:
    import urllib.request

    server = serve(tmp_path)
    try:
        url = f"http://127.0.0.1:{server.server_address[1]}/save"
        urllib.request.urlopen(urllib.request.Request(url, data=b'{"page":"order","qty":"3"}')).read()
    finally:
        server.shutdown()
    assert posted(tmp_path) == {"page": "order", "qty": "3"}
    assert BY_ID["chrome-tabs"].fault(tmp_path) == ""


def test_a_program_behind_env_is_the_one_checked() -> None:
    assert missing([["env", "A=1", "no-such-program-xyz", "--flag"]]) == ["no-such-program-xyz"]
    assert missing([["sh", "-c", "true"]]) == []
