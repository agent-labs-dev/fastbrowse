import importlib.util
from pathlib import Path

from fastbrowse.jev import NoulAnswer

_spec = importlib.util.spec_from_file_location("jev_review", Path(__file__).parents[1] / "scripts" / "jev_review.py")
assert _spec is not None and _spec.loader is not None
jev_review = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(jev_review)


def test_only_added_prose_is_judged() -> None:
    diff = "--- a/docs/x.md\n+++ b/docs/x.md\n@@ -1 +1 @@\n-old claim\n+new claim\n context\n"
    assert jev_review.added_lines(diff) == "new claim"


def test_the_changelog_is_asked_about_only_when_code_changed() -> None:
    prose = {"docs/x.md": "new claim", "CHANGELOG.md": "- **A fix.**"}
    assert set(jev_review.questions("", prose)) == {"docs/x.md"}
    asked = jev_review.questions("+def run(): ...", prose)
    assert set(asked) == {"docs/x.md", "CHANGELOG.md"}
    assert "- **A fix.**" in asked["CHANGELOG.md"].instructions


def test_a_doubt_warns_and_never_fails() -> None:
    text, warnings = jev_review.report(
        {"docs/x.md": NoulAnswer(probability=0.8), "README.md": NoulAnswer(probability=0.1)}
    )
    assert warnings == ["::warning file=docs/x.md,title=Jev review::prose not supported by diff (p=0.80)"]
    assert "| `README.md`: prose not supported by diff | 0.10 | ok |" in text
