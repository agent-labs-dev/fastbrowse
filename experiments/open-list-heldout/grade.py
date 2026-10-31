# ruff: noqa: E501
"""Grade a ledger against answers.json. A run is correct when the expected count stands alone in its answer.

Every answer holding any other standalone number is flagged for reading, because such an answer can name the right
count beside a wrong final one.
"""

import collections
import json
import pathlib
import re
import statistics
import sys

HERE = pathlib.Path(__file__).parent
answers = json.loads((HERE / "answers.json").read_text())["answers"]
WORDS = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"]
rows = [json.loads(line) for path in sys.argv[1:] for line in pathlib.Path(path).read_text().splitlines()]


def numbers(text: str) -> set[int]:
    bare = re.sub(r"\$\s?[\d,.]+|https?://\S+|\b\d+\.\d+(\.\d+)*\b", " ", text)
    found = {int(n) for n in re.findall(r"(?<![\w.,-])\d+(?![\w.,-]\d)", bare)}
    return found | {WORDS.index(w) for w in re.findall(r"\b(" + "|".join(WORDS) + r")\b", bare.lower())}


table = collections.defaultdict(list)
for row in rows:
    found = numbers(row["answer"] or "")
    row["correct"] = answers[row["task"]] in found
    row["flag"] = sorted(found - {answers[row["task"]]})
    table[row["task"], row["build"]].append(row)

print("| task | build | correct | status | steps | seconds | dollars |\n|---|---|---|---|---|---|---|")
for (task, build), group in sorted(table.items()):
    status = dict(collections.Counter(str(r["status"]) for r in group))
    print(
        f"| {task} | {build} | {sum(r['correct'] for r in group)}/{len(group)} | {status} | {[r['steps'] for r in group]} | {[round(r['seconds']) for r in group]} | {sum(r['dollars'] for r in group):.3f} |"
    )
print("\n| build | correct | median steps | median seconds | dollars |\n|---|---|---|---|---|")
for build in sorted({r["build"] for r in rows}):
    group = [r for r in rows if r["build"] == build]
    print(
        f"| {build} | {sum(r['correct'] for r in group)}/{len(group)} | {statistics.median(r['steps'] for r in group)} | {statistics.median(r['seconds'] for r in group):.0f} | {sum(r['dollars'] for r in group):.3f} |"
    )
print("\nanswers to read:")
for r in rows:
    if r["flag"] or not r["correct"]:
        print(
            r["build"],
            r["task"],
            r["repeat"],
            "correct" if r["correct"] else "WRONG",
            r["flag"],
            repr((r["answer"] or "")[:200]),
        )
