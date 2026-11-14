"""Product results and action descriptions for Parallax's browser evidence."""

import json

from pydantic import JsonValue


def result_answer(answer: str | None, data: JsonValue | None) -> str | None:
    """The structured result is the submitted answer when the caller requested a schema."""
    return answer if data is None else json.dumps(data, ensure_ascii=False)


def step_action(operation: str, outcome: str | None, note: str | None) -> str:
    """Preserve whether an action ran and the product's reason when it did not."""
    action = operation if outcome is None else f"{operation} ({outcome})"
    return action if not note else f"{action}: {note}"
