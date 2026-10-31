"""Grade on the count an answer states.

grade.py found the expected number anywhere in the answer, and its citation markers ([1], [5]) passed answers stating
another count. Its flagged list shows this; the tasks and answers stay as committed. Here the citations go first and
the count must stand in the answer's first sentence.
"""

import collections
import json
import pathlib
import re
import statistics
import sys

HERE = pathlib.Path(__file__).parent
answers = json.loads((HERE / "answers.json").read_text())["answers"]
PREFIX = "what was read."
rows = [json.loads(line) for path in sys.argv[1:] for line in pathlib.Path(path).read_text().splitlines()]


def stated(answer: str) -> set[int]:
    text = re.sub(r"\[\d+\]\(<[^>]*>\)", "", answer.split(PREFIX)[-1])
    first = re.split(r"\.\s|\n", " ".join(text.split()) + " ")[0]
    return {int(n) for n in re.findall(r"(?<![\w$.,-])\d+(?![\w.,-]*\d)", first)}


table = collections.defaultdict(list)
for row in rows:
    row["stated"] = stated(row["answer"] or "")
    row["correct"] = answers[row["task"]] in row["stated"]
    table[row["task"], row["build"]].append(row)
print("| task | build | correct | stated | status | steps | seconds | dollars |\n|---|---|---|---|---|---|---|---|")
for (task, build), group in sorted(table.items()):
    status = dict(collections.Counter(str(r["status"]) for r in group))
    said = [sorted(r["stated"]) for r in group]
    print(
        f"| {task} | {build} | {sum(r['correct'] for r in group)}/{len(group)} | {said} | {status} "
        f"| {[r['steps'] for r in group]} | {[round(r['seconds']) for r in group]} "
        f"| {sum(r['dollars'] for r in group):.3f} |"
    )
print("\n| build | correct | complete | median steps | median seconds | dollars |\n|---|---|---|---|---|---|")
for build in sorted({r["build"] for r in rows}):
    group = [r for r in rows if r["build"] == build]
    print(
        f"| {build} | {sum(r['correct'] for r in group)}/{len(group)} "
        f"| {sum(r['status'] == 'complete' for r in group)}/{len(group)} "
        f"| {statistics.median(r['steps'] for r in group)} "
        f"| {statistics.median(r['seconds'] for r in group):.0f} | {sum(r['dollars'] for r in group):.3f} |"
    )
