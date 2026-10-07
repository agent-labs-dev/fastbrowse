from fastbrowse.evals import versions


def test_committed_summary_is_generated_from_published_rows() -> None:
    assert (versions.RESULTS / "summary.json").read_text() == versions.render_summary()
