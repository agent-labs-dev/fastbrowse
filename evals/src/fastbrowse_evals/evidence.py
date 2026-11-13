"""What a judge reads of a run under `JUDGE_POLICY`: the same fields for every arm, from each arm's own record.

An arm records a page it saw either as a trace event (`page`, `read`) or as the text a step returned (`observation`,
`output`). `run_evidence` puts them all in one list, each distinct record once, and points the timeline's steps at it;
`once` then joins a page recorded with its controls and again without them. Nothing is shortened: a state too large
for one request is divided by `parts`, or by `parts_v5` under `browser-internal-opus-v5`.
"""

import hashlib
import json
import re
from collections.abc import Mapping
from typing import Any

from parallax import images
from pydantic import JsonValue

SHARED_OPERATIONS = {"type_text": "fill", "send_keys": "fill", "escalate": "reason", "reply": "reason"}
"""One word for what several arms' step logs each name their own way, after case and a `browser_` prefix."""
TOOL_NAME = re.compile(r"browser[-_]harness", re.IGNORECASE)
"""The command-line tool only one arm runs, named in its commands and their output."""
TIMELINE_STEP = ("index", "operation", "outcome", "url", "target", "choice", "command", "exit_code", "note", "file")
"""What a step says the run did. What it saw goes to `pages`; anything else on a step is that arm's own bookkeeping."""
PART_BYTES = 1_000_000
"""The most state one request carries. The judge reads 1,000,000 tokens (`FALLBACK_CONTEXT_TOKENS`), and a token covers
at least one byte of text, so a part this size fits whatever the text is."""


def shared_step(step: JsonValue) -> JsonValue:
    """One step of a blind report in the form every arm shares. What one arm alone records as a field (`facts`,
    `page_changed`) becomes text in `observation`, with its evidence intact, and each arm's own name for an operation
    becomes the shared one. Applying it twice changes nothing, so a recorded state can pass through it."""
    if not isinstance(step, dict):
        return step
    shared = {k: v for k, v in step.items() if k not in ("facts", "page_changed")}
    seen = [step["observation"]] if step.get("observation") else []
    facts = step.get("facts")
    for fact in facts if isinstance(facts, list) else []:
        if isinstance(fact, dict):
            seen.append(f"{fact.get('text')} (from {fact.get('url')}, quoted: {fact.get('quote')})")
    if isinstance(step.get("page_changed"), bool):
        seen.append("The page changed." if step["page_changed"] else "The page did not change.")
    if seen:
        shared["observation"] = "\n".join(str(line) for line in seen)
    if isinstance(operation := step.get("operation"), str):
        word = operation.lower().removeprefix("browser_")
        shared["operation"] = SHARED_OPERATIONS.get(word, word)
    for field in ("command", "output"):
        if isinstance(value := step.get(field), str):
            shared[field] = TOOL_NAME.sub("browser tool", value)
    return shared


class _Pages:
    """The distinct pages of a run, in the order it first saw them."""

    def __init__(self) -> None:
        self.pages: list[dict[str, JsonValue]] = []
        self.ids: dict[str, str] = {}

    def add(self, seen: dict[str, JsonValue]) -> str:
        key = hashlib.sha256(json.dumps(seen, sort_keys=True, default=str).encode()).hexdigest()
        if key not in self.ids:
            self.ids[key] = f"page-{len(self.pages) + 1}"
            self.pages.append({"id": self.ids[key], **seen})
        return self.ids[key]


def _text(value: JsonValue) -> str:
    return value if isinstance(value, str) else json.dumps(value, sort_keys=True)


def run_evidence(report: Mapping[str, Any], quotes: JsonValue) -> dict[str, JsonValue]:
    """The timeline, pages and citations of `report`, and which of those kinds this run has any record of."""
    pages = _Pages()
    saw: dict[int, list[str]] = {}
    typed: dict[int, JsonValue] = {}
    for event in report.get("events") or ():
        if not isinstance(event, dict):
            continue
        at = event.get("step", 0)
        if event.get("event") == "typed":
            typed[at] = event.get("value")
        elif event.get("event") in ("page", "read") and event.get("text") is not None:
            seen = {k: event[k] for k in ("url", "title", "status", "text", "controls") if event.get(k) is not None}
            saw.setdefault(at, []).append(pages.add(seen))
    logged = report.get("step_log") or report.get("decision_log")
    timeline: list[JsonValue] = []
    by_step: dict[int, list[str]] = {}
    for position, step in enumerate(logged if isinstance(logged, list) else ()):
        if not isinstance(step, dict):
            timeline.append(step)
            continue
        index = step.get("index", position)
        ids = saw.get(index, [])
        shared = shared_step({k: step.get(k) for k in (*TIMELINE_STEP, "output") if step.get(k) is not None})
        assert isinstance(shared, dict)
        for text in (shared.pop("output", None), step.get("observation")):
            if text:
                ids = [*ids, pages.add({"url": step.get("url"), "text": TOOL_NAME.sub("browser tool", _text(text))})]
        earlier = step.get("observation_same_as_step")
        ids = [*ids, *by_step.get(earlier, [])] if isinstance(earlier, int) else ids
        by_step[index] = ids
        value = typed.get(index)
        timeline.append(shared | ({"saw": list(ids)} if ids else {}) | ({"typed": value} if value is not None else {}))
    citations = [{"url": url, "quote": quote} for url, quote in quotes] if isinstance(quotes, list | tuple) else []
    return {
        "timeline": timeline,
        "pages": pages.pages,
        "citations": citations,
        "evidence_recorded": {
            "actions": bool(timeline),
            "pages": bool(pages.pages),
            "citations": bool(citations),
        },
    }


def once(state: dict[str, JsonValue]) -> dict[str, JsonValue]:
    """`state` with each page it saw listed once. A run records the same page more than once when one event carries
    its controls and another, such as a read, only its text; those become one page with its controls, and every step
    that saw either sees it. Pages whose controls differ stay apart. Nothing is dropped, and applying it twice changes
    nothing."""
    pages = state.get("pages")
    if not isinstance(pages, list):
        return state
    kept: list[dict[str, JsonValue]] = []
    by_text: dict[str, dict[str, JsonValue]] = {}
    alias: dict[JsonValue, dict[str, JsonValue]] = {}
    for page in pages:
        assert isinstance(page, dict)
        key = json.dumps({k: v for k, v in page.items() if k not in ("id", "controls")}, sort_keys=True)
        same = by_text.get(key)
        if same is not None and page.get("controls") in (None, same.get("controls")):
            alias[page["id"]] = same
            continue
        if same is not None and same.get("controls") is None:
            same["controls"] = page["controls"]
            alias[page["id"]] = same
            continue
        copy = dict(page)
        by_text.setdefault(key, copy)
        alias[page["id"]] = copy
        kept.append(copy)
    for number, page in enumerate(kept, 1):
        page["id"] = f"page-{number}"
    timeline = state.get("timeline")
    steps = [
        {**step, "saw": list(dict.fromkeys(alias[seen]["id"] if seen in alias else seen for seen in step["saw"]))}
        if isinstance(step, dict) and isinstance(step.get("saw"), list)
        else step
        for step in (timeline if isinstance(timeline, list) else ())
    ]
    return {**state, "pages": kept, **({"timeline": steps} if isinstance(timeline, list) else {})}


def parts(state: dict[str, JsonValue], limit: int = PART_BYTES) -> list[dict[str, JsonValue]]:
    """`state` as requests that each fit `limit` bytes. Everything but the pages goes in every part; the pages are
    shared out in order, filling each part before the next. A page too long for the room left continues in the next
    part, its text sliced and then its controls, so every byte of every page is sent once and none is left out."""
    if _size(state) <= limit:
        return [state]
    pages = state["pages"]
    assert isinstance(pages, list)
    room = limit - _size({**state, "pages": []})
    if room <= 0:
        # The evidence beside the pages fills a request alone. Sent whole, the judge's own window check records it
        # unjudged ("not sent") instead of a division that cannot fit.
        return [state]
    grouped: list[list[dict[str, JsonValue]]] = [[]]
    used = 0

    def free(overhead: int) -> int:
        """Bytes a piece with `overhead` bytes of its own fields can still carry in the current part, opening the next
        part when this one has no room even for those fields."""
        nonlocal used
        if used + overhead * 2 > room:
            grouped.append([])
            used = 0
        return room - used - overhead

    def place(piece: dict[str, JsonValue], label: int = 0) -> None:
        """Put `piece` in the current part, holding `label` bytes for the `continues` it is given later."""
        nonlocal used
        grouped[-1].append(piece)
        used += _size(piece) + label + 2

    for page in pages:
        assert isinstance(page, dict)
        if used + _size(page) + 2 > room and _size(page) + 2 <= room:
            grouped.append([])
            used = 0
        if used + _size(page) + 2 <= room:
            place(page)
            continue
        head = {k: v for k, v in page.items() if k not in ("text", "controls")}
        # A piece's own fields, its `continues` label at its longest with the ", " before it, and the ", " that
        # `json.dumps` writes before the next piece.
        label = _size({"continues": f"{room} of {room}"})
        overhead = _size({**head, "controls": []}) + label + 2
        pieces: list[dict[str, JsonValue]] = []
        text = str(page.get("text") or "")
        at = 0
        while at < len(text):
            space = free(overhead)
            # Most text encodes a character in one byte; shrink a slice whose escapes or multibyte characters overflow.
            take = min(len(text) - at, space)
            while (size := _size(text[at : at + take]) - 2) > space:
                take = max(1, take * space // size - 1)
            pieces.append({**head, "text": text[at : at + take]})
            place(pieces[-1], label)
            at += take
        controls = page.get("controls")
        run: list[JsonValue] = []
        space = 0
        for control in controls if isinstance(controls, list) else ():
            size = _size(control) + 2
            if run and size > space:
                pieces.append({**head, "controls": run})
                place(pieces[-1], label)
                run = []
            if not run:
                space = free(overhead)
                if size > space and used:
                    grouped.append([])
                    used = 0
                    space = room - overhead
            run.append(control)
            space -= size
        if run:
            pieces.append({**head, "controls": run})
            place(pieces[-1], label)
        if not pieces:
            place(head)
        for number, piece in enumerate(pieces if len(pieces) > 1 else (), 1):
            piece["continues"] = f"{number} of {len(pieces)}"
    return [{**state, "pages": group} for group in grouped]


def parts_v5(state: dict[str, JsonValue], limit: int = PART_BYTES) -> list[dict[str, JsonValue]]:
    """`parts` as `browser-internal-opus-v5` divides a state, kept so that policy judges as it always did: a page's
    text is cut into slices of a twelfth of a part, and every slice carries the page's whole `controls`."""
    if _size(state) <= limit:
        return [state]
    pages = state["pages"]
    assert isinstance(pages, list)
    room = limit - _size({**state, "pages": []})
    # `json.dumps` writes a character in at most twelve bytes, so a slice of this many characters always fits a part.
    slice_size = max(1, (room - 1024) // 12)
    pieces: list[dict[str, JsonValue]] = []
    for page in pages:
        assert isinstance(page, dict)
        text = str(page.get("text") or "")
        slices = [text[at : at + slice_size] for at in range(0, len(text), slice_size)] or [""]
        for number, piece in enumerate(slices, 1):
            continued = {"continues": f"{number} of {len(slices)}"} if len(slices) > 1 else {}
            pieces.append({**page, "text": piece, **continued})
    grouped: list[list[dict[str, JsonValue]]] = [[]]
    used = 0
    for piece in pieces:
        size = _size(piece) + 1
        if grouped[-1] and used + size > room:
            grouped.append([])
            used = 0
        grouped[-1].append(piece)
        used += size
    return [{**state, "pages": list(group)} for group in grouped]


def _size(value: JsonValue) -> int:
    # Images travel as attachments, so their base64 length must not split page prose into tiny requests.
    text, _ = images.split(value)
    return len(json.dumps(text).encode())
