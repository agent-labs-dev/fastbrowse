"""Advisory Jev judgments on a pull request's prose, for what no lint rule can decide.

Vale and `no_slop.py` own wording. What they cannot tell is whether the words are true to the change: whether
a changed doc claims something the code diff does not do, or whether the diff changes something a user would
notice that the changelog's Unreleased entries leave out. Jev answers each as a calibrated yes or no against the
diff, for about a hundredth of a cent.

A model's doubt is a prompt for the reviewer, never a gate: this prints a report (appended to the job summary
in CI), raises a warning annotation per doubt, and exits 0. Without a Jev key, as on a fork's pull request, it
says so and skips.

    uv run python scripts/jev_review.py --base origin/main
"""

import argparse
import asyncio
import os
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

import httpx
from pydantic import JsonValue

from fastbrowse.clients.environment import Settings
from fastbrowse.jev import JevError, NoulAnswer, NoulQuestion

ROOT = Path(__file__).resolve().parent.parent
# The gateway sheds large requests with 503s (see `TokenBudget.batch_tokens`), so each question goes alone with
# about 8k tokens of diff and prose, at about three characters a token.
_CODE_CHARS = 18_000
_PROSE_CHARS = 6_000
_MAX_DOCS = 8
_DOUBT = 0.5
# Generated from the code or the published rows, and checked by tests: nothing for a model to judge.
_GENERATED = ("docs/results/",)


def _git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def _clip(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[:limit] + f"\n[... {len(text) - limit} more characters not shown]"


def added_lines(diff: str) -> str:
    """The lines a diff adds, without its headers: the prose the change actually says."""
    return "\n".join(line[1:] for line in diff.splitlines() if line.startswith("+") and not line.startswith("+++"))


def questions(code_diff: str, prose: Mapping[str, str]) -> dict[str, NoulQuestion]:
    """One question per changed prose file, and the changelog question when the code changed."""
    asked: dict[str, NoulQuestion] = {}
    for path, added in prose.items():
        if path == "CHANGELOG.md":
            continue
        asked[path] = NoulQuestion(
            instructions=(
                f"# Prose this change adds to {path}\n{_clip(added, _PROSE_CHARS)}\n\n"
                "Read the code diff in the state. Does the added prose claim a behaviour, number, flag or name that "
                "the diff contradicts or that the diff and the prose around it give no support for?"
            ),
            true="Yes, it claims something the change does not support.",
            false="No, it is consistent with the change.",
        )
    if code_diff.strip():
        entry = prose.get("CHANGELOG.md", "")
        asked["CHANGELOG.md"] = NoulQuestion(
            instructions=(
                f"# Changelog lines this change adds\n{_clip(entry, _PROSE_CHARS) or '(none)'}\n\n"
                "Read the code diff in the state. Does it change behaviour a user of the package or its CLI would "
                "notice (not tests, internal refactors, evals or CI) that the changelog lines above do not mention?"
            ),
            true="Yes, a user-visible change is missing from the changelog.",
            false="No, the changelog covers every user-visible change, or there is none.",
        )
    return asked


def report(answers: Mapping[str, NoulAnswer]) -> tuple[str, list[str]]:
    """The Markdown report and one warning per doubt."""
    lines = ["### Jev review (advisory)", "", "| Check | Doubt | |", "| --- | --- | --- |"]
    warnings = []
    for path, answer in sorted(answers.items()):
        doubted = answer.probability >= _DOUBT
        what = "changelog misses a user-visible change" if path == "CHANGELOG.md" else "prose not supported by diff"
        lines.append(f"| `{path}`: {what} | {answer.probability:.2f} | {'check this' if doubted else 'ok'} |")
        if doubted:
            warnings.append(f"::warning file={path},title=Jev review::{what} (p={answer.probability:.2f})")
    lines += ["", "Advisory only: a doubt is worth a reviewer's look, not a failure."]
    return "\n".join(lines) + "\n", warnings


async def review(base: str) -> str:
    settings = Settings()
    if not (settings.typesafe_api_key or settings.ai_gateway_api_key):
        return "### Jev review (advisory)\n\nSkipped: no Jev key is set (a fork's pull request has none).\n"
    changed = [p for p in _git("diff", "--name-only", f"{base}...HEAD").split() if not p.startswith(_GENERATED)]
    docs = [p for p in changed if p.endswith(".md") and p != "CHANGELOG.md"][:_MAX_DOCS]
    docs += [p for p in changed if p == "CHANGELOG.md"]
    code_diff = _git("diff", f"{base}...HEAD", "--", "src") if any(p.startswith("src/") for p in changed) else ""
    prose = {p: added_lines(_git("diff", f"{base}...HEAD", "--", p)) for p in docs}
    asked = questions(code_diff, {p: text for p, text in prose.items() if text.strip() or p == "CHANGELOG.md"})
    if not asked:
        return "### Jev review (advisory)\n\nNothing to judge: the change touches no prose and no code.\n"
    state: JsonValue = {"code_diff": _clip(code_diff, _CODE_CHARS) or "(no code changes)"}
    async with httpx.AsyncClient(timeout=120) as http:
        jev = settings.jev(http)
        results = await asyncio.gather(
            *(jev.evaluate(state, {key: question}) for key, question in asked.items()), return_exceptions=True
        )
    if raised := next((r for r in results if isinstance(r, BaseException) and not isinstance(r, JevError)), None):
        raise raised
    answered = [r for r in results if not isinstance(r, BaseException)]
    answers = {key: a for r in answered for key, a in r.answers.items() if isinstance(a, NoulAnswer)}
    text, warnings = report(answers)
    if missing := [key for key in asked if key not in answers]:
        text += f"\nJev did not answer for {', '.join(f'`{key}`' for key in missing)}.\n"
    print("\n".join(warnings))
    return text


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", default="origin/main", help="the ref the pull request merges into")
    text = asyncio.run(review(parser.parse_args(argv).base))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as out:
            out.write(text)
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
