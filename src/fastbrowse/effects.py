"""What an action visibly did, told back to the policy so that a no-op cannot pass for progress."""

import hashlib
import json
from dataclasses import dataclass

from fastbrowse.page import Control, Observation

_SHOWN = 4
_TEXT = 60

SETTING_ROLES = frozenset({"option", "checkbox", "radio", "switch", "menuitemradio", "menuitemcheckbox"})
"""Clicking one of these is choosing a value, so it should leave some value, selection or address changed."""


@dataclass(frozen=True, slots=True)
class Effect:
    summary: str
    set_something: bool
    """The address, or a surviving control's label, value, checked or selected state, changed."""


def state_key(observation: Observation) -> str:
    """Identify a page state by what can be done on it, ignoring text that changes on its own (clocks, ads)."""
    controls = sorted(
        json.dumps([c.role, c.label, c.context, c.value, c.checked, c.selected, c.expanded])
        for c in observation.controls
    )
    return hashlib.sha256(json.dumps([observation.url, controls]).encode()).hexdigest()


def _short(value: object) -> str:
    text = " ".join(str(value).split()) if value is not None else ""
    text = text or "empty"
    return text if len(text) <= _TEXT else text[: _TEXT - 1] + "…"


def _listed(controls: list[Control]) -> str:
    count = f"{len(controls)} control{'' if len(controls) == 1 else 's'}"
    names = ", ".join(_short(c.label) for c in controls[:_SHOWN])
    return f"{count}: {names}" + (f" and {len(controls) - _SHOWN} more" if len(controls) > _SHOWN else "")


def _plain(text: object) -> str:
    return " ".join(str(text).split()).casefold()


def _shows(control: Control, choice: str) -> bool:
    """Whether a field now shows `choice`, whole or as the start of it ("London" for "London, United Kingdom")."""
    shown = [_plain(control.value)] if control.value else []
    if control.role == "combobox":
        shown.append(_plain(control.label))
    return any(choice in text or choice.startswith(text) for text in shown if text)


def effect(before: Observation, after: Observation, chosen: Control | None = None) -> Effect:
    """What changed from `before` to `after`. With `chosen`, the option clicked in between, a field showing it
    counts as a value set: an autocomplete choice can leave the text that was typed exactly as it was."""
    old = {c.id: c for c in before.controls}
    new = {c.id: c for c in after.controls}
    parts: list[str] = []
    navigated = before.url != after.url
    if navigated:
        parts.append(f"went to {_short(after.url)}")
    changes: list[str] = []
    setting = False
    # A picker can open as an overlay with its own copy of the field, so the field the choice fills is a
    # different element before and after. A control unmatched by id is compared with the one earlier control
    # sharing its role and label, if exactly one did.
    by_name: dict[tuple[str, str], list[Control]] = {}
    for control in before.controls:
        by_name.setdefault((control.role, control.label.strip()), []).append(control)
    for key, now in new.items():
        was = old.get(key)
        if was is None:
            namesakes = by_name.get((now.role, now.label.strip()), [])
            if len(namesakes) != 1 or namesakes[0].id in new:
                continue
            was = namesakes[0]
        for name in ("label", "value", "checked", "selected", "expanded"):
            a, b = getattr(was, name), getattr(now, name)
            if (_plain(a) != _plain(b)) if name == "label" else (a != b):
                changes.append(f"{_short(was.label)} {name}: {_short(a)} -> {_short(b)}")
                # A menu opening or closing is not a value set.
                setting = setting or name != "expanded"
    if changes:
        parts.append(
            "changed "
            + "; ".join(changes[:_SHOWN])
            + (f" and {len(changes) - _SHOWN} more" if len(changes) > _SHOWN else "")
        )
    shown = [c for key, c in new.items() if key not in old]
    hidden = [c for key, c in old.items() if key not in new]
    if shown:
        parts.append(f"showed {_listed(shown)}")
    if hidden:
        parts.append(f"removed {_listed(hidden)}")
    if chosen is not None:
        choice = _plain(chosen.label)
        setting = setting or any(c.role not in SETTING_ROLES and _shows(c, choice) for c in after.controls)
    summary = "; ".join(parts) or "nothing visible changed"
    # A submit a form refuses moves nothing or reopens a picker, and says why only in the fields it marks: Search
    # on a round trip with no return date reopened the date picker, and the run clicked Done and Search in turn.
    blocking = [c for c in after.controls if c.blocking]
    if blocking and not (navigated or setting):
        summary += f"; fields the form still needs: {_listed(blocking)}"
    return Effect(summary=summary, set_something=navigated or setting)
