import importlib.util
from pathlib import Path

import pytest

from fastbrowse.jev import NoulAnswer

_spec = importlib.util.spec_from_file_location("jev_review", Path(__file__).parents[1] / "scripts" / "jev_review.py")
assert _spec is not None and _spec.loader is not None
jev_review = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(jev_review)


def test_only_added_prose_is_judged() -> None:
    diff = "--- a/docs/x.md\n+++ b/docs/x.md\n@@ -1 +1 @@\n-old claim\n+new claim\n context\n"
    assert jev_review.added_lines(diff) == "new claim"


def test_the_changelog_is_asked_about_only_for_a_user_visible_change() -> None:
    prose = {"docs/x.md": "new claim", "CHANGELOG.md": "- **A fix.**"}
    assert set(jev_review.questions(prose, user_visible=False)) == {"docs/x.md"}
    asked = jev_review.questions(prose, user_visible=True)
    assert set(asked) == {"docs/x.md", "CHANGELOG.md"}
    assert "- **A fix.**" in asked["CHANGELOG.md"].instructions


@pytest.mark.parametrize(("path", "visible"), [("pyproject.toml", True), (".github/workflows/ci.yml", False)])
def test_prose_is_judged_against_every_change_but_only_the_package_is_user_visible(
    monkeypatch: pytest.MonkeyPatch, path: str, visible: bool
) -> None:
    diffs = {("diff", "--name-only", "b...h"): f"{path}\nAGENTS.md\n", ("diff", "b...h", "--", path): "+changed\n"}
    diffs["diff", "b...h", "--", "AGENTS.md"] = "+a claim about it\n"
    monkeypatch.setattr(jev_review, "_git", lambda *args: diffs[args])
    assert jev_review.changes("b...h") == ("+changed\n", visible, {"AGENTS.md": "a claim about it"})


def test_a_doubt_warns_and_never_fails() -> None:
    text, warnings = jev_review.report(
        {"docs/x.md": NoulAnswer(probability=0.8), "README.md": NoulAnswer(probability=0.1)}
    )
    assert warnings == ["::warning file=docs/x.md,title=Jev review::prose not supported by diff (p=0.80)"]
    assert "| `README.md`: prose not supported by diff | 0.10 | ok |" in text
