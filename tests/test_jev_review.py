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
    assert "- **A fix.**" in asked["CHANGELOG.md"][0].instructions


def test_long_prose_and_diffs_are_split_into_parts_that_keep_every_character() -> None:
    text = "".join(f"line {i}\n" for i in range(50)) + "x" * 25
    split = jev_review.parts(text, 20)
    assert "".join(split) == text
    assert all(len(part) <= 20 for part in split)
    assert jev_review.parts("", 20) == [""]
    asked = jev_review.questions({"docs/x.md": "claim\n" * 2000}, user_visible=False)
    assert len(asked["docs/x.md"]) == 2


def test_a_claim_any_diff_part_supports_stands_and_a_change_any_changelog_part_mentions_is_covered() -> None:
    # Rows are prose parts, columns diff parts.
    assert jev_review.combined("docs/x.md", [[0.9, 0.1], [0.2, 0.3]]) == 0.2
    assert jev_review.combined("docs/x.md", [[0.9, 0.1], [0.8, 0.7]]) == 0.7
    assert jev_review.combined("CHANGELOG.md", [[0.9, 0.1], [0.2, 0.3]]) == 0.2


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
