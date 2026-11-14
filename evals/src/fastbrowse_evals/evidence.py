"""Product action descriptions for Parallax's browser evidence."""


def step_action(operation: str, outcome: str | None, note: str | None) -> str:
    """Preserve whether an action ran and the product's reason when it did not."""
    action = operation if outcome is None else f"{operation} ({outcome})"
    return action if not note else f"{action}: {note}"
