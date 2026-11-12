"""Measure recorded decision states without sending their contents to a provider.

Read bench cassettes or run traces. Report exact serialized state characters by question set and top-level field,
beside the provider's input tokens for the whole request. Providers do not report a state-only token count.
"""

import argparse
import gzip
import json
from collections import Counter
from pathlib import Path
from typing import Any


def characters(value: Any) -> int:
    """Characters in the JSON encoding used for model requests, including JSON punctuation and escaping."""
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")))


def profile(paths: list[Path]) -> dict[str, Any]:
    """Aggregate recorded exchanges, retaining unanswered requests in the size counts."""
    groups: dict[str, dict[str, Any]] = {}
    for path in paths:
        opener = gzip.open if path.suffix == ".gz" else open
        with opener(path, "rt", encoding="utf-8") as lines:
            for line in lines:
                record = json.loads(line)
                request = record.get("request")
                if (
                    not isinstance(request, dict)
                    or "state" not in request
                    or not isinstance(request.get("questions"), dict)
                ):
                    continue
                if path.suffix != ".gz" and record.get("event") != "http":
                    continue
                response = record.get("body") if path.suffix == ".gz" else record.get("response")
                if isinstance(response, str):
                    try:
                        response = json.loads(response)
                    except ValueError:
                        response = None
                usage = response.get("usage", {}) if isinstance(response, dict) else {}
                usage = usage if isinstance(usage, dict) else {}
                kind = ",".join(request["questions"])
                group = groups.setdefault(
                    kind,
                    {"requests": 0, "state_chars": 0, "input_tokens": 0, "metered_requests": 0, "fields": Counter()},
                )
                group["requests"] += 1
                state = request["state"]
                group["state_chars"] += characters(state)
                if isinstance(state, dict):
                    for key, value in state.items():
                        group["fields"][key] += characters(value)
                tokens = usage.get("input_tokens", usage.get("inputTokens"))
                if isinstance(tokens, int) and not isinstance(tokens, bool):
                    group["input_tokens"] += tokens
                    group["metered_requests"] += 1
    return {"kinds": groups, "token_scope": "input_tokens includes the state and questions"}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=Path, help="run trace JSONL or bench cassette JSONL.gz")
    args = parser.parse_args()
    print(json.dumps(profile(args.paths), indent=2))


if __name__ == "__main__":
    main()
