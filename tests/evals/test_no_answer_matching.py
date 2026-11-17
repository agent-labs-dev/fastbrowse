"""An eval verdict on an agent's answer comes from the LLM judge alone, never from matching its text.

A substring, regex, keyword list or case fold over an answer grades the wording rather than the fact. State read
from a mock site and protocol data are fine; the answer is not. This scans the harness sources for the shapes that
match one: a string method or `in` test on an expression naming `answer`, or an `re` call given one.
"""

import ast
from pathlib import Path

ROOT = Path(__file__).parents[2]
SOURCES = (ROOT / "src/fastbrowse/evals", ROOT / "evals/src")
STRING_METHODS = {
    "lower", "casefold", "upper", "startswith", "endswith", "find", "rfind", "index", "count", "split",
    "splitlines", "partition", "replace", "removeprefix", "removesuffix", "translate",
}  # fmt: skip
RE_FUNCTIONS = {"search", "match", "fullmatch", "findall", "finditer", "sub", "subn", "split"}


def names_an_answer(node: ast.AST) -> bool:
    return any(
        (isinstance(n, ast.Name) and n.id == "answer") or (isinstance(n, ast.Attribute) and n.attr == "answer")
        for n in ast.walk(node)
    )


def matches_answer(node: ast.AST) -> bool:
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        if node.func.attr in STRING_METHODS and names_an_answer(node.func.value):
            return True
        is_re = isinstance(node.func.value, ast.Name) and node.func.value.id == "re"
        if is_re and node.func.attr in RE_FUNCTIONS and any(names_an_answer(a) for a in node.args):
            return True
    return (
        isinstance(node, ast.Compare)
        and any(isinstance(op, ast.In | ast.NotIn) for op in node.ops)
        and (names_an_answer(node))
    )


def offenders(source: str) -> list[int]:
    return [getattr(node, "lineno", 0) for node in ast.walk(ast.parse(source)) if matches_answer(node)]


def test_the_detector_finds_each_shape() -> None:
    for bad in (
        "expected = want.lower() in answer.lower()",
        "if task.answer.lower() not in text: pass",
        "re.search('1815', result.answer or '')",
        "lines = (result.answer or '').splitlines()",
        "'x' in outcome.answer",
    ):
        assert offenders(bad), bad
    assert not offenders("passed = await judge(jev, task=t, answer=result.answer, claims=claims)")


def test_no_harness_grader_matches_answer_text() -> None:
    found = [
        f"{path.relative_to(ROOT)}:{line}"
        for source in SOURCES
        for path in sorted(source.rglob("*.py"))
        for line in offenders(path.read_text())
    ]
    assert found == [], "grade an answer with a judge claim (fastbrowse.evals.judge), never by matching its text"
