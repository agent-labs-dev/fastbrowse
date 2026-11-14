"""Does Jev lean toward the option offered first? Read the recorded choices, and on request ask them again reversed.

With no spend flag the probe reads Jev requests from bench cassettes and run traces and reports, for each choice
question, the place each option was offered in and the probability Jev gave it. That costs nothing, but in a gate
whose options always come in the same order a place cannot be told apart from what the option means.

With `--yes-spend` it sends each recorded request to Jev twice: once as recorded, to measure how much a repeat moves
on its own, and once with every choice's options in reverse order (the swap test of Zheng et al. 2023, "Judging
LLM-as-a-Judge with MT-Bench and Chatbot Arena"). Per option set it reports the shift, the mean probability of an
option offered first minus the same option offered last, and how often the pick changes under reversal and on the
plain repeat. It changes no agent behaviour.
"""

from __future__ import annotations

import argparse
import copy
import json
import math
import os
import re
import sys
from collections import defaultdict
from collections.abc import Iterable, Iterator
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from fastbrowse.evals.replay import JEV_MOUNT, TARGET_ID, Cassette, Forward, openrouter_forward, response_cost

JEV_PATH = f"{JEV_MOUNT}/v1/systemone"
CASSETTES = Path("bench/cassettes")
FIXED = 4
"""An option set this size or smaller is a fixed gate; a longer one is a page's own list, grouped as one."""
LISTS = f"more than {FIXED} options"
DIGITS = re.compile(r"\d+")
BLANKED = "<image>"

type Recorded = tuple[str, dict[str, Any], dict[str, Any] | None]


@dataclass
class Row:
    """One choice question as it was offered: its options in order, and the probability Jev gave each."""

    source: str
    question: str
    group: str
    options: list[str]
    probabilities: dict[str, float]


@dataclass
class Trial:
    """One choice question asked again in its recorded order and reversed."""

    source: str
    question: str
    group: str
    options: list[str]
    recorded: str | None
    same: dict[str, float]
    reversed: dict[str, float]


def _choices(body: dict[str, Any]) -> dict[str, dict[str, Any]]:
    questions = body.get("questions")
    if not isinstance(questions, dict):
        return {}
    return {
        name: question
        for name, question in questions.items()
        if isinstance(question, dict)
        and question.get("type") == "choice"
        and isinstance(question.get("criteria"), dict)
    }


def _trace_requests(path: Path) -> Iterator[Recorded]:
    with path.open(encoding="utf-8") as lines:
        for line in lines:
            if not line.strip():
                continue
            event = json.loads(line)
            if event.get("event") != "http" or not isinstance(event.get("request"), dict):
                continue
            response = event.get("response")
            yield str(path), event["request"], response if isinstance(response, dict) else None


def _cassette_requests(path: Path) -> Iterator[Recorded]:
    for exchange in Cassette.load(path).exchanges:
        if not exchange.path.startswith(JEV_MOUNT + "/") or not isinstance(exchange.request, dict):
            continue
        try:
            response = json.loads(exchange.body)
        except ValueError:
            response = None
        yield str(path), exchange.request, response if isinstance(response, dict) else None


def requests(paths: Iterable[Path]) -> Iterator[Recorded]:
    """(source, Jev request body, recorded response) for every recorded Jev request that asks a choice.

    A path ending in `.gz` is a bench cassette; any other is a run trace, whose `http` events hold each request.
    """
    for path in paths:
        found = _cassette_requests(path) if path.suffix == ".gz" else _trace_requests(path)
        for source, body, response in found:
            if _choices(body):
                yield source, body, response


def option_set(options: Iterable[str]) -> str:
    """The name of an option set: its keys sorted, a Chrome target id as TARGET and the numbers in ids such as c12 or
    main:3 as N."""
    if len(options := list(options)) > FIXED:
        return LISTS
    return "/".join(sorted({DIGITS.sub("N", TARGET_ID.sub("TARGET", option)) for option in options}))


def _probabilities(response: dict[str, Any] | None, question: str) -> dict[str, float]:
    answers = (response or {}).get("answers")
    answer = answers.get(question) if isinstance(answers, dict) else None
    odds = answer.get("probabilities") if isinstance(answer, dict) else None
    if not isinstance(odds, dict):
        return {}
    return {str(key): float(value) for key, value in odds.items() if isinstance(value, int | float)}


def positions(body: dict[str, Any], response: dict[str, Any] | None, source: str = "") -> list[Row]:
    """Each choice question's options in the order offered, with the recorded probability of each."""
    rows: list[Row] = []
    for name, question in _choices(body).items():
        options = list(question["criteria"])
        rows.append(Row(source, name, option_set(options), options, _probabilities(response, name)))
    return rows


def reversed_request(body: dict[str, Any]) -> dict[str, Any]:
    """A copy of the request with every choice question's options in reverse order, and nothing else changed."""
    flipped = copy.deepcopy(body)
    for question in _choices(flipped).values():
        question["criteria"] = dict(reversed(list(question["criteria"].items())))
    return flipped


def _blanked(body: dict[str, Any]) -> bool:
    """Whether the recording replaced an image asset's pixels with a placeholder, so the request cannot be resent."""
    assets = body.get("assets")
    items = assets.values() if isinstance(assets, dict) else assets if isinstance(assets, list) else []
    return any(isinstance(asset, dict) and asset.get("data_base64") == BLANKED for asset in items)


def _send(forward: Forward, body: dict[str, Any]) -> tuple[dict[str, Any] | None, float]:
    status, _, text = forward(JEV_PATH, json.dumps(body).encode())
    cost = response_cost(text) or 0.0
    if status != 200:
        return None, cost
    try:
        parsed = json.loads(text)
    except ValueError:
        return None, cost
    return (parsed if isinstance(parsed, dict) else None), cost


def _pick(odds: dict[str, float]) -> str | None:
    return max(odds, key=lambda option: odds[option]) if odds else None


def probe(
    recorded: Iterable[Recorded], forward: Forward, *, only_fixed: bool = False, max_dollars: float | None = None
) -> tuple[list[Trial], float]:
    """Ask each recorded request again as recorded and reversed. Returns the trials and the dollars spent.

    Before each pair the next cost is estimated as twice the recorded one, or the last pair's when none was recorded,
    and the probe stops if that would take the total past `max_dollars`. With a cap, a request that has no estimate,
    because nothing was recorded and no pair has been sent yet, is left out. So is a request whose images the
    recording blanked, since it cannot be asked again as it was.
    """
    trials: list[Trial] = []
    spent = 0.0
    last_pair: float | None = None
    for source, body, response in recorded:
        questions = {
            name: question
            for name, question in _choices(body).items()
            if not only_fixed or len(question["criteria"]) <= FIXED
        }
        if not questions or _blanked(body):
            continue
        once = response_cost(json.dumps(response)) if response else None
        estimate = last_pair if once is None else 2 * once
        if max_dollars is not None:
            if estimate is None:
                continue
            if spent + estimate > max_dollars:
                break
        same, same_cost = _send(forward, body)
        flipped, flipped_cost = _send(forward, reversed_request(body))
        last_pair = same_cost + flipped_cost
        spent += last_pair
        for name, question in questions.items():
            options = list(question["criteria"])
            trials.append(
                Trial(
                    source=source,
                    question=name,
                    group=option_set(options),
                    options=options,
                    recorded=_pick(_probabilities(response, name)),
                    same=_probabilities(same, name),
                    reversed=_probabilities(flipped, name),
                )
            )
    return trials, spent


def _mean(values: list[float]) -> float | None:
    return sum(values) / len(values) if values else None


def report(trials: list[Trial]) -> list[dict[str, Any]]:
    """Per option set: n, the first-minus-last shift, and the pick's flip rate under reversal and on a plain repeat.

    The shift pairs the option offered first with itself offered last in the reversed request, and the option offered
    last with itself offered first, so what an option means is held fixed and only its place moves.
    """
    groups: dict[str, list[Trial]] = defaultdict(list)
    for trial in trials:
        if trial.same and trial.reversed:
            groups[trial.group].append(trial)
    rows: list[dict[str, Any]] = []
    for group, members in sorted(groups.items()):
        shifts: list[float] = []
        for trial in members:
            first, last = trial.options[0], trial.options[-1]
            if first in trial.same and first in trial.reversed:
                shifts.append(trial.same[first] - trial.reversed[first])
            if last in trial.same and last in trial.reversed:
                shifts.append(trial.reversed[last] - trial.same[last])
        repeats = [trial for trial in members if trial.recorded is not None]
        rows.append(
            {
                "group": group,
                "n": len(members),
                "shift": _mean(shifts),
                "reversal_flips": _mean([float(_pick(t.same) != _pick(t.reversed)) for t in members]),
                "noise_flips": _mean([float(t.recorded != _pick(t.same)) for t in repeats]),
                "noise_n": len(repeats),
            }
        )
    return rows


def recorded_report(rows: list[Row]) -> list[dict[str, Any]]:
    """Per option set, from the recordings alone: how likely the first and the last option were, and how often the
    first was picked. In a fixed gate these mix place with meaning; only the reversal separates them."""
    groups: dict[str, list[Row]] = defaultdict(list)
    for row in rows:
        if row.probabilities:
            groups[row.group].append(row)
    return [
        {
            "group": group,
            "n": len(members),
            "first": _mean([row.probabilities.get(row.options[0], 0.0) for row in members]),
            "last": _mean([row.probabilities.get(row.options[-1], 0.0) for row in members]),
            "first_picked": _mean([float(_pick(row.probabilities) == row.options[0]) for row in members]),
        }
        for group, members in sorted(groups.items())
    ]


def _cell(value: Any) -> str:
    return "" if value is None else f"{value:.3f}" if isinstance(value, float) else str(value)


def markdown(rows: list[dict[str, Any]]) -> str:
    if not rows:
        return "No choice questions found.\n"
    columns = list(rows[0])
    lines = ["| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
    lines.extend("| " + " | ".join(_cell(row[column]) for column in columns) + " |" for row in rows)
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cassettes", nargs="*", type=Path, help=f"bench cassettes (default: every one in {CASSETTES})")
    parser.add_argument("--trace", action="append", type=Path, default=[], help="a run trace JSONL; repeatable")
    parser.add_argument("--out", type=Path, required=True, help="directory for report.json and report.md")
    parser.add_argument("--yes-spend", action="store_true", help="ask Jev again, paid, with OPENROUTER_API_KEY")
    parser.add_argument("--max-dollars", type=float, help="stop before the paid total would pass this; required")
    parser.add_argument("--only", choices=["fixed"], help=f"ask again only option sets of {FIXED} or fewer")
    args = parser.parse_args(argv)
    paid = args.max_dollars is not None or args.only is not None
    if paid and not args.yes_spend:
        print("--max-dollars and --only ask Jev again, which costs money: pass --yes-spend too.", file=sys.stderr)
        return 2
    key = os.environ.get("OPENROUTER_API_KEY")
    if args.yes_spend and not key:
        print("--yes-spend needs OPENROUTER_API_KEY.", file=sys.stderr)
        return 2
    if args.yes_spend and (args.max_dollars is None or not math.isfinite(args.max_dollars) or args.max_dollars < 0):
        print("--yes-spend needs a --max-dollars cap of 0 or more.", file=sys.stderr)
        return 2
    paths = (args.cassettes or sorted(CASSETTES.glob("*.jsonl.gz"))) + args.trace
    recorded = list(requests(paths))
    rows = [row for source, body, response in recorded for row in positions(body, response, source)]
    result: dict[str, Any] = {"requests": len(recorded), "recorded": recorded_report(rows)}
    text = "## Recorded\n\n" + markdown(result["recorded"])
    if args.yes_spend and key:
        trials, spent = probe(
            recorded, openrouter_forward(key), only_fixed=args.only == "fixed", max_dollars=args.max_dollars
        )
        result |= {"spent": spent, "probe": report(trials), "trials": [asdict(trial) for trial in trials]}
        text += f"\n## Asked again, reversed (${spent:.4f})\n\n" + markdown(result["probe"])
    result["rows"] = [asdict(row) for row in rows]
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    (args.out / "report.md").write_text(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
