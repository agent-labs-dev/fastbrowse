from fastbrowse.evals import versions


def test_committed_summary_is_generated_from_published_rows() -> None:
    assert (versions.RESULTS / "summary.json").read_text() == versions.render_summary()


def _row(arm: str, basis: str | None) -> dict[str, object]:
    return {
        "arm": arm,
        "task": "t",
        "task_version": 1,
        "suite": "live",
        "suite_version": "1",
        "repeat": 0,
        "status": "complete",
        "normalized_status": "done",
        "passed": True,
        "seconds": 10.0,
        "dollars": 0.1,
        "timing_basis": basis,
        "run": {"run_id": "r", "run_started": "2026-01-01T00:00:00+00:00"},
    }


def test_a_comparison_across_timing_bases_reports_no_seconds() -> None:
    same = versions.summary(
        [("1.0.0", [_row("fastbrowse", "session-ready-to-answer"), _row("browser-use", "session-ready-to-answer")])]
    )
    mixed = versions.summary([("1.0.0", [_row("fastbrowse", "session-ready-to-answer"), _row("browser-use", None)])])
    assert same.releases[0].arms["browser-use"].seconds.median == 10.0
    assert mixed.releases[0].arms["fastbrowse"].seconds.median is None
    assert mixed.releases[0].arms["browser-use"].seconds.median is None
    assert mixed.releases[0].arms["fastbrowse"].dollars.median == 0.1
