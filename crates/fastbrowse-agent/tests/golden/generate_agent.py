"""Run whole agent loops in Python against a scripted site and scripted models, for Rust to replay.

    uv run python crates/fastbrowse-agent/tests/golden/generate_agent.py

Each scenario drives `Agent.run` end to end. The page is a `FakeSite`, a small state machine of screens: observing
and capturing read the screen the site is on, and an action or a navigation moves it to the screen the script names,
so a read and an observation sent side by side see the same page whatever order they run in. Jev and the LLM answer
from scripts and record every request as the bytes it would be sent as, with the answer given. `tests/agent.rs` builds
the same site, answers each request by looking its bytes up (so send order may differ), fails on any request Python
never made, and compares the final `RunResult` and the step events.

A `Control.operations` frozenset is written in hash order, which changes per process; prompts that dump a control
write it sorted here, as Rust writes it.
"""

import asyncio
import hashlib
import json
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

from pydantic import BaseModel, JsonValue

from fastbrowse import agent as agent_module
from fastbrowse.agent import Agent
from fastbrowse.config import Config
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation, LLMError, Message
from fastbrowse.models import (
    Attachment,
    Authorization,
    CostBasis,
    CostComponent,
    CostLine,
    Limits,
    LLMPurpose,
    Operation,
    StepEvent,
    StepOutcome,
)
from fastbrowse.page import Action, ActResult, BrowserError, Capture, Control, Observation
from fastbrowse.safety import ScopedSecrets
from fastbrowse.telemetry import Ledger

HERE = Path(__file__).parent


NOW = "2026-09-26T20:00:00Z"
FREE_JEV = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)

_dump = Control.model_dump


def _sorted_dump(self: Control, **kwargs: Any) -> Any:
    dumped = _dump(self, **kwargs)
    if isinstance(dumped, dict) and isinstance(dumped.get("operations"), list):
        dumped["operations"] = sorted(dumped["operations"])
    return dumped


Control.model_dump = _sorted_dump  # ty: ignore[invalid-assignment]
# The scripted site never changes on its own, so waiting for it to would only spend wall time.
agent_module._INTERSTITIAL_SECONDS = 0.0


def compact(value: Any) -> str:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


class FakeSite:
    """Screens, the addresses that open them and the actions that move between them."""

    def __init__(self, site: Mapping[str, Any]) -> None:
        self.site = site
        self.screen: str = site["start"]
        self.back: list[str] = []
        self.seen: dict[str, int] = {}
        self.visits: dict[str, int] = {}

    def _screen(self) -> Mapping[str, Any]:
        return self.site["screens"][self.screen]

    @property
    def artifacts(self) -> tuple[()]:
        return ()

    async def observe(self) -> Observation:
        # A screen that `settles` turns into another once it has been observed `after` times, as a 429 page does
        # when the site lets the client back in.
        settles = self._screen().get("settles")
        if settles is not None:
            self.seen[self.screen] = self.seen.get(self.screen, 0) + 1
            if self.seen[self.screen] > settles["after"]:
                self.screen = settles["to"]
        return Observation.model_validate(self._screen()["observation"])

    async def document_changed(self, observation: Observation) -> bool:
        return False

    async def navigate(self, url: str, *, back_to: str | None = None) -> None:
        screen = self.site["urls"].get(url)
        if screen is None:
            raise BrowserError(f"no page at {url}")
        # A list is one screen per visit, the last repeated: a reload that mints a new document each time.
        if isinstance(screen, list):
            visit = self.visits.get(url, 0)
            self.visits[url] = visit + 1
            screen = screen[min(visit, len(screen) - 1)]
        self.back.append(self.screen)
        self.screen = screen

    async def capture(self) -> Capture:
        return Capture.model_validate(self._screen()["capture"])

    async def act(self, action: Action, observation: Observation) -> ActResult:
        if action.operation is Operation.NAVIGATE and action.url is not None:
            await self.navigate(action.url)
            return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)
        if action.operation is Operation.BACK and self.back:
            self.screen = self.back.pop()
            return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)
        for rule in self.site.get("actions", ()):
            if (
                rule["screen"] == self.screen
                and rule["operation"] == action.operation.value
                and (rule.get("target") in (None, action.target_id))
            ):
                moved = rule.get("next", self.screen)
                if moved != self.screen:
                    self.back.append(self.screen)
                self.screen = moved
                return ActResult.model_validate(rule.get("result", {"outcome": "executed", "page_changed": True}))
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=False)

    async def redrawn(self, observation: Observation, timeout_seconds: float, *, target_id: str | None = None) -> bool:
        return False

    async def screenshot(self) -> bytes:
        return b"png"

    def withhold_frames(self, withheld: bool) -> None:
        return None

    # A scripted site reports no lifecycle events, so a wait on one ends at once with nothing new.
    def lifecycle_mark(self) -> int:
        return 0

    async def wait_for_lifecycle(self, mark: int, seconds: float) -> bool:
        return False

    async def address(self) -> str:
        return self._screen()["observation"]["url"]

    async def response_status(self) -> int | None:
        return self._screen()["observation"].get("response_status")


Respond = Callable[[str, Mapping[str, Question]], dict[str, Any] | None]


class RecordingJev:
    """ScriptedJev with picks that can change per call: `pick[key]` is a value, or a list consumed one per ask."""

    def __init__(self, calls: list[dict[str, Any]], pick: Mapping[str, Any], noul: float, nouls: Mapping[str, Any]):
        self.calls = calls
        self.pick = {key: list(value) if isinstance(value, list) else value for key, value in pick.items()}
        self.noul = noul
        # No page is a wall unless the scenario says so.
        walls = {"bot_check": 0.0, "login_required": 0.0, **nouls}
        self.nouls = {key: list(value) if isinstance(value, list) else value for key, value in walls.items()}

    @staticmethod
    def _take(table: dict[str, Any], key: str, default: Any) -> Any:
        value = table.get(key, default)
        if isinstance(value, list):
            return value.pop(0) if len(value) > 1 else value[0]
        return value

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        answers: dict[str, Any] = {}
        for key, question in questions.items():
            match question:
                case ChoiceQuestion():
                    options = list(question.criteria)
                    choice = self._take(self.pick, key, options[0])
                    if choice not in options:
                        choice = options[0]
                    answers[key] = ChoiceAnswer(
                        choice=choice, probabilities={o: float(o == choice) for o in options}, confidence=0.9
                    )
                case NoulQuestion():
                    answers[key] = NoulAnswer(probability=self._take(self.nouls, key, self.noul))
                case _:
                    raise AssertionError(question)
        evaluation = Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE_JEV)
        self.calls.append(
            {
                "jev": {
                    "state": compact(state),
                    "questions": compact({k: q.model_dump(mode="json") for k, q in questions.items()}),
                },
                "response": {"evaluation": evaluation.model_dump(mode="json")},
            }
        )
        return evaluation


DEFAULT_REPLIES: dict[str, list[JsonValue]] = {
    "Shortcut": [{"url": None}],
    "_AnswerDraft": [{"claims": []}],
    "LLMVerdict": [{"missing": [], "complete": False}],
    "_ReadResponse": [{"claims": [], "answered": False}],
    "_Recovery": [{"diagnosis": "No way forward", "next_subgoal": "Stop", "give_up": True}],
}
"""What a scenario that does not script a response model is answered: no shortcut, no evidence, give up."""


class RecordingLLM:
    """Answers each response model from its own queue, in the order Python asks; the last answer repeats."""

    def __init__(self, calls: list[dict[str, Any]], replies: Mapping[str, Sequence[JsonValue]]) -> None:
        self.calls = calls
        self.replies = {name: list(values) for name, values in {**DEFAULT_REPLIES, **replies}.items()}

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        queue = self.replies.get(schema.__name__)
        if not queue:
            raise AssertionError(f"no scripted {schema.__name__} for {purpose.value}")
        data = queue.pop(0) if len(queue) > 1 else queue[0]
        if isinstance(data, dict) and "raise" in data:
            # A scripted provider failure: the request is recorded, and no receipt comes back.
            self.calls.append({"llm": self._request(purpose, messages, schema, max_output_tokens), "response": data})
            raise LLMError(str(data["raise"]))
        cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
        self.calls.append(
            {
                "llm": self._request(purpose, messages, schema, max_output_tokens),
                "response": {"data": data, "cost": cost.model_dump(mode="json")},
            }
        )
        return Generation(data=schema.model_validate(data), cost=cost)

    @staticmethod
    def _request(
        purpose: LLMPurpose, messages: Sequence[Message], schema: type[BaseModel], max_output_tokens: int
    ) -> dict[str, Any]:
        return {
            "purpose": purpose.value,
            "messages": compact([message.model_dump(mode="json") for message in messages]),
            "schema_name": schema.__name__,
            "schema": json.dumps(schema.model_json_schema()),
            "max_output_tokens": max_output_tokens,
        }


# --- page builders ------------------------------------------------------------------------------------------------


def control(cid: str, role: str, label: str, *operations: str, **fields: Any) -> dict[str, Any]:
    return {"id": cid, "frame_id": None, "role": role, "label": label, "operations": sorted(operations), **fields}


def screen(url: str, title: str, text: str, controls: Sequence[dict[str, Any]] = (), **fields: Any) -> dict[str, Any]:
    blocks = []
    start = 0
    parts = [part for part in text.split("\n\n") if part]
    for i, part in enumerate(parts):
        kind = "heading" if i == 0 else "paragraph"
        blocks.append({"source_id": f"s{i}", "kind": kind, "frame_id": None, "start": start, "end": start + len(part)})
        start += len(part) + 2
    body = "\n\n".join(parts)
    key = hashlib.sha256(f"{url}|{title}|{text}|{compact(list(controls))}".encode()).hexdigest()[:16]
    observation = {
        "url": url,
        "title": title,
        "page_key": key,
        "captured_at": NOW,
        "document_key": fields.pop("document_key", key),
        "controls": list(controls),
        "omitted_controls": 0,
        "viewport_text": body,
        "tabs": [],
        **fields,
    }
    capture = {
        "url": url,
        "title": title,
        "captured_at": NOW,
        "sha256": hashlib.sha256(body.encode()).hexdigest(),
        "text": body,
        "blocks": blocks,
    }
    return {"observation": observation, "capture": capture}


def plan(*requirements: tuple[str, str, str], answer: bool = True, checks: Sequence[str] = ()) -> dict[str, Any]:
    return {
        "requirements": [{"id": rid, "text": text, "kind": kind} for rid, text, kind in requirements],
        "answer_expected": answer,
        "answer_checks": list(checks),
    }


def recovery(diagnosis: str, subgoal: str, **fields: Any) -> dict[str, Any]:
    return {"diagnosis": diagnosis, "next_subgoal": subgoal, "give_up": False, **fields}


# --- scenarios ----------------------------------------------------------------------------------------------------


def scenarios() -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []

    def scenario(
        name: str, task: str, site: dict[str, Any], llm: Mapping[str, Sequence[JsonValue]], **options: Any
    ) -> None:
        found.append({"name": name, "task": task, "site": site, "llm": dict(llm), **options})

    home = "https://shop.test/"
    login = screen(
        home,
        "Sign in",
        "Sign in\n\nEnter your password to continue.",
        [
            control("user", "textbox", "Email", "fill", input_type="email"),
            control("pass", "textbox", "Password", "fill", input_type="password", sensitive=True),
            control("go", "button", "Sign in", "click"),
        ],
    )
    scenario(
        "a sign-in wall with a password field stops needs_login",
        "Find my last order total on shop.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": login},
            "urls": {home: "login"},
        },
        {
            "Plan": [plan(("r1", "Find the last order total", "information"), checks=["the order total"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
        },
        start=home,
        jev={"pick": {}, "nouls": {"login_required": 0.95}},
    )

    challenge = screen(home, "Just a moment", "Checking your browser\n\nThis takes a few seconds.")
    scenario(
        "a bot check with nothing to act on is reloaded once then blocks with its words",
        "Find the price of the blue mug on shop.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "wall": challenge},
            "urls": {home: "wall"},
        },
        {
            "Plan": [plan(("r1", "Find the blue mug's price", "information"), checks=["the price"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
        },
        start=home,
        jev={"pick": {}, "nouls": {"bot_check": 0.97}},
    )

    # Each load mints a new document with its own ray id, as a Cloudflare interstitial does.
    reloaded = [
        screen(home, "Just a moment", f"Checking your browser\n\nRay ID: {n}f3a.", document_key=f"d{n}")
        for n in range(3)
    ]
    scenario(
        "a bot check that survives its reload as a new document is reloaded once per address, then blocks",
        "Find the price of the blue mug on shop.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), **{f"wall{n}": w for n, w in enumerate(reloaded)}},
            "urls": {home: ["wall0", "wall1", "wall2"]},
        },
        {
            "Plan": [plan(("r1", "Find the blue mug's price", "information"), checks=["the price"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
        },
        start=home,
        jev={"pick": {}, "nouls": {"bot_check": 0.97}},
    )

    cart = screen(
        "https://shop.test/cart",
        "Cart",
        "Cart\n\nBlue mug, $12.",
        [control("buy", "button", "Place order", "click", submit_semantics="submit")],
    )
    scenario(
        "an irreversible click without authorization stops needs_confirmation",
        "Place the order in my cart on shop.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "cart": cart},
            "urls": {"https://shop.test/cart": "cart"},
        },
        {"Plan": [plan(("a1", "Place the order", "action"), answer=False)], "Shortcut": [{"url": None}]},
        start="https://shop.test/cart",
        jev={"pick": {"operation": "click"}, "noul": 0.9},
    )

    scenario(
        "a confident authorized click is not asked about and a failed finish recovers until stuck",
        "Place the order in my cart on shop.test",
        {
            "start": "blank",
            "screens": {
                "blank": screen("about:blank", "", ""),
                "cart": cart,
                "done": screen("https://shop.test/thanks", "Thanks", "Thanks\n\nOrder 42 placed."),
            },
            "urls": {"https://shop.test/cart": "cart"},
            "actions": [{"screen": "cart", "operation": "click", "target": "buy", "next": "done"}],
        },
        {
            "Plan": [plan(("a1", "Place the order", "action"), answer=False)],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_Recovery": [{"diagnosis": "Nothing left", "next_subgoal": "Stop", "give_up": True}],
        },
        start="https://shop.test/cart",
        authorization={"irreversible_actions": True},
        jev={"pick": {"operation": ["click", "done"]}, "noul": 0.1},
        limits={"max_steps": 6},
    )

    search = screen(
        "https://books.test/",
        "Books",
        "Books\n\nSearch the catalogue.",
        [control("q", "textbox", "Search", "fill", "enter", form_id="f1", input_name="q")],
    )
    results = screen(
        "https://books.test/search?q=dune",
        "Results",
        "Results for dune\n\nDune by Frank Herbert, 1965.",
        [control("q", "textbox", "Search", "fill", "enter", value="dune")],
    )
    scenario(
        "the field writer types a search, Enter submits it, and the read answer completes with its citation",
        "Search books.test for dune and tell me who wrote it",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": search, "results": results},
            "urls": {"https://books.test/": "home"},
            "actions": [
                {
                    "screen": "home",
                    "operation": "fill",
                    "target": "q",
                    "next": "home",
                    "result": {"outcome": "executed", "page_changed": False},
                },
                {"screen": "home", "operation": "enter", "target": "q", "next": "results"},
            ],
        },
        {
            "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_FieldText": [{"missing": False, "text": "dune"}],
            "_ReadResponse": [{"claims": [], "answered": False}],
            "_Recovery": [{"diagnosis": "The page lacks it", "next_subgoal": "Stop", "give_up": True}],
        },
        start="https://books.test/",
        jev={"pick": {"operation": ["fill", "enter", "read"]}, "noul": 0.1},
    )

    scenario(
        "a step limit ends budget_exceeded",
        "Search books.test for dune and tell me who wrote it",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": search},
            "urls": {"https://books.test/": "home"},
        },
        {
            "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_FieldText": [{"missing": False, "text": "dune"}],
        },
        start="https://books.test/",
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    scenario(
        "no start and no address in the task stops needs_input",
        "Tell me something interesting",
        {"start": "blank", "screens": {"blank": screen("about:blank", "", "")}, "urls": {}},
        {
            "Plan": [plan(("r1", "Tell something interesting", "information"), checks=["something"])],
            "StartPage": [{"url": None}],
        },
        choose_start=True,
    )

    missing = screen("https://shop.test/missing", "Not found", "Not found\n\nNo such page.", response_status=404)
    scenario(
        "an HTTP 404 goes to recovery, which gives up, and the run stops stuck",
        "Read the returns policy at https://shop.test/missing",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "missing": missing},
            "urls": {"https://shop.test/missing": "missing"},
        },
        {
            "Plan": [plan(("r1", "Find the returns policy", "information"), checks=["the returns policy"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_Recovery": [{"diagnosis": "The page is gone", "next_subgoal": "Stop", "give_up": True}],
        },
        start="https://shop.test/missing",
        jev={"pick": {}, "noul": 0.1},
    )

    downloads = screen(
        "https://files.test/",
        "Releases",
        "Releases\n\nVersion 2.0 is out.",
        [control("zip", "link", "Download 2.0", "click", href="/v2.zip", download=True)],
    )
    scenario(
        "a download the task rules out is refused and recovery gives up",
        "Find the latest version on files.test without downloading anything",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": downloads},
            "urls": {"https://files.test/": "home"},
        },
        {
            "Plan": [plan(("r1", "Find the latest version", "information"), checks=["the latest version"])],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_ReadResponse": [{"claims": [], "answered": False}],
            "_Recovery": [{"diagnosis": "Stuck", "next_subgoal": "Stop", "give_up": True}],
        },
        start="https://files.test/",
        jev={"pick": {"operation": "click", "read_assessment": "absent"}, "noul": 0.95},
    )

    project = screen("https://pypi.test/project/httpx/", "httpx", "httpx\n\nhttpx 0.28.1 released.", [])
    front = screen("https://pypi.test/", "PyPI", "PyPI\n\nFind packages.", [control("s", "textbox", "Search", "fill")])
    scenario(
        "an accepted shortcut opens in place of the start page and recovery gives up",
        "Find the latest httpx version on pypi.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "front": front, "project": project},
            "urls": {"https://pypi.test/": "front", "https://pypi.test/project/httpx/": "project"},
        },
        {
            "Plan": [plan(("r1", "Find the latest httpx version", "information"), checks=["the latest httpx version"])],
            "Shortcut": [{"url": "https://pypi.test/project/httpx/"}],
            "_ReadResponse": [{"claims": [], "answered": False}],
            "_Recovery": [{"diagnosis": "Not here", "next_subgoal": "Stop", "give_up": True}],
        },
        start="https://pypi.test/",
        jev={"pick": {"operation": "read"}, "noul": 0.1},
    )

    form = screen(
        "https://forms.test/",
        "Contact",
        "Contact us\n\nTell us who you are.",
        [
            control("first", "textbox", "First name", "fill", form_id="c"),
            control("last", "textbox", "Last name", "fill", form_id="c"),
            control("send", "button", "Send", "click", form_id="c", submit_semantics="submit"),
        ],
    )
    scenario(
        "the form writer fills two fields in one batch, then a step limit ends the run",
        "Fill the contact form on forms.test as Ada Lovelace",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "form": form},
            "urls": {"https://forms.test/": "form"},
            "actions": [
                {
                    "screen": "form",
                    "operation": "fill",
                    "result": {"outcome": "executed", "page_changed": False, "form_unchanged": True},
                }
            ],
        },
        {
            "Plan": [plan(("a1", "Fill the contact form as Ada Lovelace", "action"), answer=False)],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_FormText": [
                {
                    "fields": [
                        {"id": "first", "missing": False, "text": "Ada"},
                        {"id": "last", "missing": False, "text": "Lovelace"},
                    ]
                }
            ],
        },
        start="https://forms.test/",
        jev={"pick": {"operation": "fill", "target:fill": "first"}, "noul": 0.1},
        limits={"max_steps": 2},
    )

    filled = screen(
        "https://forms.test/",
        "Contact",
        "Contact us\n\nTell us who you are.",
        [
            control("first", "textbox", "First name", "fill", form_id="c", value="Ada"),
            control("last", "textbox", "Last name", "fill", form_id="c", value="Lovelace"),
            control("send", "button", "Send", "click", form_id="c", submit_semantics="submit"),
        ],
    )
    scenario(
        "typing a value a field already holds is no progress, and the next decision is told to submit instead",
        "Fill the contact form on forms.test as Ada Lovelace",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "form": filled},
            "urls": {"https://forms.test/": "form"},
            "actions": [
                {
                    "screen": "form",
                    "operation": "fill",
                    "result": {"outcome": "executed", "page_changed": False, "form_unchanged": True},
                }
            ],
        },
        {
            "Plan": [plan(("a1", "Fill the contact form as Ada Lovelace", "action"), answer=False)],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_FieldText": [{"missing": False, "text": "Ada"}],
        },
        start="https://forms.test/",
        jev={"pick": {"operation": "fill", "target:fill": "first"}, "noul": 0.1},
        limits={"max_steps": 2},
    )

    signin = screen(
        "https://acme.test/login",
        "Log in",
        "Log in\n\nWelcome back.",
        [
            control(
                "pw",
                "textbox",
                "Password",
                "fill",
                input_type="password",
                sensitive=True,
                frame_origin="https://acme.test",
            )
        ],
    )
    scenario(
        "a stored secret is typed at fill time and never shown to a model",
        "Log in to acme.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": signin},
            "urls": {"https://acme.test/login": "login"},
        },
        {"Plan": [plan(("a1", "Log in", "action"), answer=False)], "Shortcut": [{"url": None}]},
        start="https://acme.test/login",
        secrets={"password": ["hunter2", ["https://acme.test"]]},
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    picker = screen(
        "https://travel.test/",
        "Trips",
        "Trips\n\nChoose a class.",
        [control("cls", "combobox", "Class", "select", options=["Economy", "Business", "First"])],
    )
    scenario(
        "a select asks Jev which option, then a step limit ends the run",
        "Choose business class on travel.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "trip": picker},
            "urls": {"https://travel.test/": "trip"},
        },
        {"Plan": [plan(("a1", "Choose business class", "action"), answer=False)], "Shortcut": [{"url": None}]},
        start="https://travel.test/",
        jev={"pick": {"operation": "select", "pick": "1"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    scenario(
        "an uncertain decision goes to recovery, which names a control the run then takes",
        "Choose business class on travel.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "trip": picker},
            "urls": {"https://travel.test/": "trip"},
        },
        {
            "Plan": [plan(("a1", "Choose business class", "action"), answer=False)],
            "Shortcut": [{"url": None}],
            "_AnswerDraft": [{"claims": []}],
            "_Recovery": [recovery("Unsure which control", "Select business class", control=0, operation="select")],
        },
        start="https://travel.test/",
        jev={"pick": {"operation": "select", "pick": "1"}, "noul": 0.1, "confidence": 0.3},
        limits={"max_steps": 1},
    )
    article = screen(
        "https://wiki.test/Dune",
        "Dune",
        "Dune (novel)\n\nDune is a 1965 novel by Frank Herbert.",
        [control("talk", "link", "Talk", "click", href="/Talk:Dune")],
    )
    lookup_site = {
        "start": "blank",
        "screens": {"blank": screen("about:blank", "", ""), "article": article},
        "urls": {"https://wiki.test/Dune": "article"},
    }
    lookup_llm: dict[str, list[JsonValue]] = {
        "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
        "LLMVerdict": [{"missing": [], "complete": True}],
    }
    scenario(
        "a read by Jev's choice, a done check the verifier settles, and a cited answer complete with frames",
        "Who wrote Dune? Use https://wiki.test/Dune",
        lookup_site,
        lookup_llm,
        start="https://wiki.test/Dune",
        step_frames=True,
        jev={"pick": {"operation": "read", "r1": "c1"}, "noul": 0.5, "nouls": {"draft_needs_writing": 0.0}},
    )
    menu_article = screen(
        "https://wiki.test/Dune",
        "Dune",
        "Dune (novel)\n\nDune is a 1965 novel by Frank Herbert.",
        [control("menu", "button", "Menu", "click")],
    )
    scenario(
        "a wall Jev sees on a page offering no sign-in is not one, and the run reads and completes",
        "Who wrote Dune? Use https://wiki.test/Dune",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "article": menu_article},
            "urls": {"https://wiki.test/Dune": "article"},
        },
        lookup_llm,
        start="https://wiki.test/Dune",
        jev={
            "pick": {"operation": "read", "r1": "c1"},
            "noul": 0.5,
            "nouls": {"draft_needs_writing": 0.0, "login_required": 0.95},
        },
    )
    scenario(
        "an until check that refuses the final address sends the run back to recovery",
        "Who wrote Dune? Use https://wiki.test/Dune",
        lookup_site,
        lookup_llm,
        start="https://wiki.test/Dune",
        until=["https://wiki.test/elsewhere"],
        jev={"pick": {"operation": "read", "r1": "c1"}, "noul": 0.95, "nouls": {"draft_needs_writing": 0.0}},
    )
    scenario(
        "the LLM reader cites a block and Jev's done check accepts",
        "Who wrote Dune? Use https://wiki.test/Dune",
        lookup_site,
        {
            **lookup_llm,
            "_ReadResponse": [
                {
                    "claims": [
                        {
                            "requirement_id": "r1",
                            "text": "Frank Herbert wrote Dune",
                            "cite": {"first": "s1", "last": "s1"},
                        }
                    ],
                    "answered": True,
                }
            ],
        },
        start="https://wiki.test/Dune",
        jev={
            "pick": {"operation": "read", "r1": "synthesis"},
            "noul": 0.95,
            "nouls": {"draft_needs_writing": 0.0, "unmet_r1": 0.0},
        },
    )

    # A sort redraws the fare list within its document. When the redraw shows only what the run read, the read it
    # owes is answered from the notes; when it drops the fare the notes quote, the read is paid for.
    fares_url = "https://air.test/fares"
    unsorted = screen(
        fares_url,
        "Fares",
        "Fares\n\n$320 1 stop",
        [control("sort", "button", "Sort by price", "click")],
        document_key="fares",
    )
    sorted_fares = screen(
        fares_url,
        "Fares",
        "Fares\n\n$320 1 stop",
        [control("sort", "button", "Sorted by price", "click")],
        document_key="fares",
    )
    emptied = screen(
        fares_url,
        "Fares",
        "Fares\n\nNo fares match",
        [control("sort", "button", "Sorted by price", "click")],
        document_key="fares",
    )
    fare_read = {
        "claims": [{"requirement_id": "r1", "text": "The fare is $320 1 stop", "cite": {"first": "s1", "last": "s1"}}],
        "answered": True,
    }
    for name, redrawn in (
        ("a click that redraws only what the run read owes no second read", sorted_fares),
        ("a click that drops a fare the notes quote is read again", emptied),
    ):
        scenario(
            name,
            "Find the fare on https://air.test/fares",
            {
                "start": "blank",
                "screens": {"blank": screen("about:blank", "", ""), "fares": unsorted, "sorted": redrawn},
                "urls": {fares_url: "fares"},
                "actions": [{"screen": "fares", "operation": "click", "target": "sort", "next": "sorted"}],
            },
            {
                "Plan": [
                    plan(
                        ("r1", "Find the fare", "information"),
                        ("a1", "Sort the fares by price", "action"),
                        checks=["the fare"],
                    )
                ],
                "_ReadResponse": [fare_read, {"claims": [], "answered": False}],
                "LLMVerdict": [{"missing": [], "complete": True}],
            },
            start=fares_url,
            jev={
                "pick": {"operation": ["read", "click", "done"], "click_target": "sort", "r1": "synthesis"},
                "noul": 0.95,
                "nouls": {
                    "draft_needs_writing": 0.0,
                    "unmet_r1": 0.0,
                    "unsupported_0": 0.0,
                    "contradicted_0": 0.0,
                    "output_missing_0": 0.0,
                    "output_missing_1": 0.0,
                },
            },
            authorization={"irreversible_actions": True},
            limits={"max_steps": 6},
        )

    first = screen(
        "https://shop.test/list?page=1",
        "Mugs",
        "Mugs, page 1\n\nRed mug $5\n\nBlue mug $7",
        [control("next", "link", "Next", "click", href="/list?page=2", next_page=True)],
    )
    second = screen("https://shop.test/list?page=2", "Mugs", "Mugs, page 2\n\nGreen mug $3", [])
    scenario(
        "a list that goes on is paged by code, read to its end and left unverified by its audit",
        "Find the cheapest mug across every page of https://shop.test/list?page=1",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "p1": first, "p2": second},
            "urls": {"https://shop.test/list?page=1": "p1", "https://shop.test/list?page=2": "p2"},
            "actions": [{"screen": "p1", "operation": "click", "target": "next", "next": "p2"}],
        },
        {
            "Plan": [
                plan(("r1", "Find the cheapest mug across every page", "information"), checks=["the cheapest mug"])
            ],
            "_ReadResponse": [
                {
                    "claims": [],
                    "answered": False,
                    "continues": [
                        {
                            "requirement_id": "r1",
                            "records": [{"first": "s1", "last": "s1"}, {"first": "s2", "last": "s2"}],
                        }
                    ],
                },
                {
                    "claims": [
                        {
                            "requirement_id": "r1",
                            "text": "The green mug at $3 is cheapest",
                            "cite": {"first": "s1", "last": "s1"},
                        }
                    ],
                    "answered": True,
                },
            ],
        },
        start="https://shop.test/list?page=1",
        jev={"pick": {"operation": "read", "r1": "synthesis"}, "noul": 0.95, "nouls": {"draft_needs_writing": 0.0}},
        limits={"max_steps": 8},
    )

    dialog = screen(
        "https://mail.test/draft",
        "Draft",
        "Draft\n\nDiscard this draft?",
        [],
        dialog={"kind": "confirm", "message": "Discard this draft?"},
    )
    scenario(
        "accepting a confirm dialog is gated, and an unauthorized commit stops needs_confirmation",
        "Discard my draft on https://mail.test/draft",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "draft": dialog},
            "urls": {"https://mail.test/draft": "draft"},
        },
        {"Plan": [plan(("a1", "Discard the draft", "action"), answer=False)]},
        start="https://mail.test/draft",
        jev={"pick": {"operation": "dialog", "pick": "accept"}, "noul": 0.9},
    )

    scenario(
        "a caller-supplied address is checked as a destination and opened by NAVIGATE",
        "Compare https://wiki.test/Dune with https://books.test/",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "article": article, "home": search},
            "urls": {"https://wiki.test/Dune": "article", "https://books.test/": "home"},
        },
        {"Plan": [plan(("a1", "Open both pages", "action"), answer=False)]},
        start="https://wiki.test/Dune",
        jev={"pick": {"operation": ["navigate", "done"]}, "noul": 0.95, "nouls": {"irreversible": 0.0}},
        limits={"max_steps": 3},
    )

    sign_in = screen(
        "https://shop.test/login",
        "Sign in",
        "Sign in\n\nThe code is on the Phone page.",
        [control("phone", "link", "Phone", "click", href="https://phone.test/")],
    )
    phone = screen("https://phone.test/", "Phone", "Phone\n\nCode: 123456.", [])
    scenario(
        "NAVIGATE may return to the caller's start page, which the task does not spell out",
        "Sign in to the shop. The Phone page shows the code.",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": sign_in, "phone": phone},
            "urls": {"https://shop.test/login": "login"},
            "actions": [{"screen": "login", "operation": "click", "target": "phone", "next": "phone"}],
        },
        {"Plan": [plan(("a1", "Sign in", "action"), answer=False)]},
        start="https://shop.test/login",
        jev={"pick": {"operation": ["click", "navigate", "done"]}, "noul": 0.95, "nouls": {"irreversible": 0.0}},
        limits={"max_steps": 3},
    )

    redirect = screen("https://elsewhere.test/", "Elsewhere", "Elsewhere\n\nNot the site.", [])
    scenario(
        "a shortcut that lands on another site returns to the start page",
        "Find the latest httpx version on pypi.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "front": front, "away": redirect},
            "urls": {"https://pypi.test/": "front", "https://pypi.test/project/httpx/": "away"},
        },
        {
            "Plan": [plan(("r1", "Find the latest httpx version", "information"), checks=["the latest httpx version"])],
            "Shortcut": [{"url": "https://pypi.test/project/httpx/"}],
        },
        start="https://pypi.test/",
        jev={"pick": {"operation": "escalate"}, "noul": 0.1},
    )

    gated = screen(
        "https://forum.test/thread",
        "Thread",
        "Thread\n\nNo replies yet.\n\nLog in to reply.",
        [control("login", "link", "Log in", "click", href="/login")],
    )
    scenario(
        "a sign-in prompt beside readable content is read before it is called a wall",
        "How many replies does https://forum.test/thread have?",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "thread": gated},
            "urls": {"https://forum.test/thread": "thread"},
        },
        {
            "Plan": [plan(("r1", "Find how many replies the thread has", "information"), checks=["the reply count"])],
        },
        start="https://forum.test/thread",
        jev={"pick": {"operation": "read", "r1": "absent"}, "noul": 0.1, "nouls": {"login_required": 0.95}},
    )

    blank_deep = screen("https://site.test/deep/path", "", "", [])
    site_front = screen(
        "https://site.test", "Site", "Site\n\nWelcome.", [control("a", "link", "About", "click", href="/about")]
    )
    scenario(
        "a start page worked out from the task that opens nothing falls back to the front page",
        "Find the about page of site.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "deep": blank_deep, "front": site_front},
            "urls": {"https://site.test/deep/path": "deep", "https://site.test": "front"},
        },
        {
            "Plan": [plan(("a1", "Open the about page", "action"), answer=False)],
            "StartPage": [{"url": "https://site.test/deep/path"}],
        },
        choose_start=True,
        jev={"pick": {"operation": "escalate"}, "noul": 0.1},
    )

    # --- loops drawn from the unit tests of single behaviours ----------------------------------------------------

    index = screen(
        "https://wiki.test/",
        "Wiki",
        "Wiki\n\nArticles on novels.",
        [control("dune", "link", "Dune", "click", href="/Dune")],
    )
    leaving = screen(
        "https://wiki.test/Dune",
        "Dune",
        "Dune (novel)\n\nDune is a 1965 novel by Frank Herbert.",
        [control("home", "link", "Home", "click", href="/")],
        can_go_back=True,
    )
    scenario(
        "a back off a page not yet read reads it first, and that read completes the run",
        "Who wrote Dune? Start at https://wiki.test/",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "index": index, "article": leaving},
            "urls": {"https://wiki.test/": "index"},
            "actions": [{"screen": "index", "operation": "click", "target": "dune", "next": "article"}],
        },
        {
            "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
        },
        start="https://wiki.test/",
        jev={"pick": {"operation": ["click", "back", "back"]}, "noul": 0.1},
        limits={"max_steps": 4},
    )

    toggle = screen(
        "https://app.test/",
        "App",
        "App\n\nOpen the settings panel.",
        [control("open", "button", "Settings", "click")],
    )
    toggle_site = {
        "start": "blank",
        "screens": {"blank": screen("about:blank", "", ""), "app": toggle},
        "urls": {"https://app.test/": "app"},
    }
    toggle_llm = {"Plan": [plan(("a1", "Open the settings panel", "action"), answer=False)]}
    scenario(
        "a click that keeps coming back stale goes to recovery, which gives up",
        "Open the settings panel on https://app.test/",
        {
            **toggle_site,
            "actions": [{"screen": "app", "operation": "click", "result": {"outcome": "stale", "page_changed": False}}],
        },
        toggle_llm,
        start="https://app.test/",
        jev={"pick": {"operation": "click"}, "noul": 0.1},
        limits={"max_steps": 3},
    )
    scenario(
        "a covered click counts as idle, so picking it again goes to recovery, which gives up",
        "Open the settings panel on https://app.test/",
        {
            **toggle_site,
            "actions": [
                {"screen": "app", "operation": "click", "result": {"outcome": "covered", "page_changed": False}}
            ],
        },
        toggle_llm,
        start="https://app.test/",
        jev={"pick": {"operation": "click"}, "noul": 0.1},
        limits={"max_steps": 6},
    )
    scenario(
        "a click that changed nothing is not taken again: the repeat goes to recovery, which gives up",
        "Open the settings panel on https://app.test/",
        toggle_site,
        toggle_llm,
        start="https://app.test/",
        jev={"pick": {"operation": "click"}, "noul": 0.1},
        limits={"max_steps": 8},
    )
    scenario(
        "an armed repetition tripwire recovers, and repeating after it ends the run stuck",
        "Open the settings panel on https://app.test/",
        {
            **toggle_site,
            "actions": [
                {"screen": "app", "operation": "click", "result": {"outcome": "executed", "page_changed": True}}
            ],
        },
        toggle_llm,
        start="https://app.test/",
        config={"stall": {"tripwires": "armed", "repeated_actions": 2, "unchanged_actions": 5}},
        jev={"pick": {"operation": "click"}, "noul": 0.1},
        limits={"max_steps": 8},
    )

    menu = screen(
        "https://shop.test/menu",
        "Shop",
        "Shop\n\nHover the menu to see departments.",
        [control("dept", "button", "Departments", "hover", "click")],
    )
    scenario(
        "a hover is taken and then the step limit ends the run",
        "Open the departments menu on https://shop.test/menu",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "menu": menu},
            "urls": {"https://shop.test/menu": "menu"},
        },
        {"Plan": [plan(("a1", "Open the departments menu", "action"), answer=False)]},
        start="https://shop.test/menu",
        jev={"pick": {"operation": "hover"}, "noul": 0.1},
        limits={"max_steps": 2},
    )

    long_page = screen(
        "https://news.test/story",
        "Story",
        "Story\n\nThe council met on Monday.\n\nIt voted to close the bridge.",
        [control("more", "link", "More", "click", href="/more")],
    )
    scenario(
        "a scroll over a page not yet read reads it first, and that read completes the run",
        "What did the council vote on? See https://news.test/story",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "story": long_page},
            "urls": {"https://news.test/story": "story"},
        },
        {
            "Plan": [plan(("r1", "Find what the council voted on", "information"), checks=["the vote"])],
        },
        start="https://news.test/story",
        jev={"pick": {"operation": "scroll"}, "noul": 0.1},
        limits={"max_steps": 3},
    )

    apply = screen(
        "https://jobs.test/apply",
        "Apply",
        "Apply\n\nAttach your CV.",
        [control("cv", "textbox", "CV", "upload", input_type="file")],
    )
    scenario(
        "an upload asks which attachment, sends it, and the step limit ends the run",
        "Attach my CV on https://jobs.test/apply",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "apply": apply},
            "urls": {"https://jobs.test/apply": "apply"},
            "actions": [
                {"screen": "apply", "operation": "upload", "result": {"outcome": "executed", "page_changed": False}}
            ],
        },
        {"Plan": [plan(("a1", "Attach the CV", "action"), answer=False)]},
        start="https://jobs.test/apply",
        attachments=[
            {"name": "cover.txt", "mime_type": "text/plain", "content": "Dear team"},
            {"name": "cv.pdf", "mime_type": "application/pdf", "content": "%PDF-1.4"},
        ],
        jev={"pick": {"operation": "upload", "pick": "cv.pdf"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    signup = screen(
        "https://news.test/signup",
        "Newsletter",
        "Newsletter\n\nSign up for updates.",
        [control("email", "textbox", "Email", "fill", input_type="email")],
    )
    scenario(
        "a caller input chosen for a field is typed as given, and the step limit ends the run",
        "Sign up for the newsletter on https://news.test/signup",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "signup": signup},
            "urls": {"https://news.test/signup": "signup"},
            "actions": [
                {"screen": "signup", "operation": "fill", "result": {"outcome": "executed", "page_changed": False}}
            ],
        },
        {"Plan": [plan(("a1", "Sign up for the newsletter", "action"), answer=False)]},
        start="https://news.test/signup",
        inputs={"email": "ada@example.test", "name": "Ada"},
        jev={"pick": {"operation": "fill", "pick": "input:email"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    pin = screen(
        "https://bank.test/login",
        "Bank",
        "Bank\n\nEnter your PIN.",
        [
            control(
                "pin", "textbox", "PIN", "fill", input_type="password", sensitive=True, frame_origin="https://bank.test"
            )
        ],
    )
    scenario(
        "a sensitive field with several stored secrets asks which, types it, and the step limit ends the run",
        "Log in to https://bank.test/login",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": pin},
            "urls": {"https://bank.test/login": "login"},
        },
        {"Plan": [plan(("a1", "Log in", "action"), answer=False)]},
        start="https://bank.test/login",
        secrets={"password": ["hunter2", ["https://bank.test"]], "pin": ["4321", ["https://bank.test"]]},
        jev={"pick": {"operation": "fill", "pick": "pin"}, "noul": 0.1},
        limits={"max_steps": 1},
    )

    busy = screen("https://wiki.test/Dune", "Too many requests", "Too many requests\n\nSlow down.", response_status=429)
    busy["settles"] = {"after": 1, "to": "article"}
    scenario(
        "an HTTP 429 that clears while waited on is read once it does",
        "Who wrote Dune? Use https://wiki.test/Dune",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "busy": busy, "article": article},
            "urls": {"https://wiki.test/Dune": "busy"},
        },
        lookup_llm,
        start="https://wiki.test/Dune",
        interstitial=True,
        jev={"pick": {"operation": "read", "r1": "c1"}, "noul": 0.5, "nouls": {"draft_needs_writing": 0.0}},
    )

    unauthorized = screen(
        "https://admin.test/settings",
        "Sign in",
        "Sign in\n\nThis page needs an account.",
        [control("sso", "button", "Sign in with SSO", "click")],
        response_status=401,
    )
    scenario(
        "a 401 on a task that inspects access is read, and recovery giving up there ends needs_login",
        "Check whether https://admin.test/settings is restricted to signed-in users",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "wall": unauthorized},
            "urls": {"https://admin.test/settings": "wall"},
        },
        {
            "Plan": [
                {
                    **plan(
                        ("r1", "Find whether the settings page needs sign-in", "information"),
                        checks=["whether sign-in is required"],
                    ),
                    "inspect_access": True,
                }
            ],
        },
        start="https://admin.test/settings",
        jev={"pick": {"operation": "read", "r1": "absent"}, "noul": 0.1, "nouls": {"login_required": 0.95}},
    )

    scenario(
        "an LLM that fails during recovery ends the run with an error",
        "Read the returns policy at https://shop.test/missing",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "missing": missing},
            "urls": {"https://shop.test/missing": "missing"},
        },
        {
            "Plan": [plan(("r1", "Find the returns policy", "information"), checks=["the returns policy"])],
            "_Recovery": [{"raise": "provider returned 500"}],
        },
        start="https://shop.test/missing",
        jev={"pick": {}, "noul": 0.1},
    )

    scenario(
        "an LLM call budget ends the run budget_exceeded",
        "Search books.test for dune and tell me who wrote it",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": search},
            "urls": {"https://books.test/": "home"},
            "actions": [
                {"screen": "home", "operation": "fill", "result": {"outcome": "executed", "page_changed": False}}
            ],
        },
        {
            "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
            "_FieldText": [{"missing": False, "text": "dune"}],
        },
        start="https://books.test/",
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
        limits={"max_llm_calls": 3},
    )
    scenario(
        "a Jev call budget ends the run budget_exceeded",
        "Open the settings panel on https://app.test/",
        toggle_site,
        toggle_llm,
        start="https://app.test/",
        jev={"pick": {"operation": "click"}, "noul": 0.1},
        limits={"max_jev_calls": 2},
    )

    contact = screen(
        "https://shop.test/contact",
        "Contact",
        "Contact\n\nSend us your message.",
        [control("send", "button", "Send message", "click", submit_semantics="submit")],
    )
    sent = screen(
        "https://shop.test/contact/sent",
        "Sent",
        "Sent\n\nYour message has been sent.",
        response_status=503,
    )
    scenario(
        "an authorized send that lands on a 503 goes to recovery, and giving up there ends unavailable",
        "Send the message on https://shop.test/contact",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "contact": contact, "sent": sent},
            "urls": {"https://shop.test/contact": "contact"},
            "actions": [{"screen": "contact", "operation": "click", "target": "send", "next": "sent"}],
        },
        {"Plan": [plan(("a1", "Send the message", "action"), answer=False)]},
        start="https://shop.test/contact",
        authorization={"irreversible_actions": True},
        jev={"pick": {"operation": ["click", "done"]}, "noul": 0.95, "nouls": {"irreversible": 0.95}},
        limits={"max_steps": 4},
    )
    panel = screen(
        "https://app.test/panel",
        "Panel",
        "Panel\n\nThree switches.",
        [
            control("a", "button", "Alpha", "click"),
            control("b", "button", "Beta", "click"),
            control("c", "button", "Gamma", "click"),
            control("d", "button", "Delta", "click"),
        ],
    )
    scenario(
        "clicks on different buttons that change nothing trip the stall rule and recovery gives up",
        "Turn on the panel at https://app.test/panel",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "panel": panel},
            "urls": {"https://app.test/panel": "panel"},
        },
        {"Plan": [plan(("a1", "Turn on the panel", "action"), answer=False)]},
        start="https://app.test/panel",
        jev={"pick": {"operation": "click", "click_target": ["a", "b", "c", "d"]}, "noul": 0.1},
        limits={"max_steps": 8},
    )

    thanks = screen("https://shop.test/thanks", "Thanks", "Thanks\n\nOrder 42 placed.")
    scenario(
        "an authorized order whose receipt is read has its click classified at the finish",
        "Place the order in my cart on shop.test and tell me its number",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "cart": cart, "done": thanks},
            "urls": {"https://shop.test/cart": "cart"},
            "actions": [{"screen": "cart", "operation": "click", "target": "buy", "next": "done"}],
        },
        {
            "Plan": [
                plan(
                    ("a1", "Place the order", "action"),
                    ("r1", "Find the order number", "information"),
                    checks=["the order number"],
                )
            ],
            "_ReadResponse": [
                {
                    "claims": [
                        {"requirement_id": "r1", "text": "Order 42 placed", "cite": {"first": "s1", "last": "s1"}}
                    ],
                    "answered": False,
                }
            ],
            "LLMVerdict": [{"missing": [], "complete": True}],
        },
        start="https://shop.test/cart",
        authorization={"irreversible_actions": True},
        jev={
            "pick": {"operation": ["click", "read", "done"]},
            "noul": 0.95,
            "nouls": {"unmet_a1": 0.0, "unmet_r1": 0.0, "draft_needs_writing": 0.0},
        },
        limits={"max_steps": 6},
    )

    down = screen("https://wiki.test/Dune", "Unavailable", "Service unavailable\n\nTry later.", response_status=503)
    mirror = screen(
        "https://mirror.test/Dune",
        "Dune",
        "Dune (novel)\n\nDune is a 1965 novel by Frank Herbert.",
    )
    scenario(
        "an HTTP 503 that does not clear goes to recovery, which opens the other address the task gave and completes",
        "Who wrote Dune? Use https://wiki.test/Dune or https://mirror.test/Dune",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "down": down, "mirror": mirror},
            "urls": {"https://wiki.test/Dune": "down", "https://mirror.test/Dune": "mirror"},
        },
        {
            **lookup_llm,
            "_Recovery": [
                recovery("The page is down", "Open the mirror", operation="navigate", url="https://mirror.test/Dune")
            ],
        },
        start="https://wiki.test/Dune",
        jev={
            "pick": {"operation": ["escalate", "read"], "r1": "c1"},
            "noul": 0.5,
            "nouls": {"draft_needs_writing": 0.0, "destination": 0.95},
        },
    )

    signed_out = {"diagnosis": "A sign-in blocks it", "next_subgoal": "Sign in", "give_up": True, "needs_login": True}
    for name, label, href in (
        ("recovery that gives up at a localized sign-in prompt ends the run needing login", "Se connecter", None),
        ("recovery that gives up at a federated sign-in ends the run needing login", "Continue", "/sso/start"),
        ("recovery that says a sign-in blocks a page offering none ends the run stuck", "Continue", None),
    ):
        fields = {} if href is None else {"href": href}
        portal = screen(
            "https://portal.test/",
            "Portal",
            "Portal\n\nMembers area.",
            [control("enter", "link" if href else "button", label, "click", **fields)],
        )
        scenario(
            name,
            "Download my invoice from https://portal.test/",
            {
                "start": "blank",
                "screens": {"blank": screen("about:blank", "", ""), "portal": portal},
                "urls": {"https://portal.test/": "portal"},
            },
            {"Plan": [plan(("a1", "Download the invoice", "action"), answer=False)], "_Recovery": [signed_out]},
            start="https://portal.test/",
            jev={"pick": {"operation": "click"}, "noul": 0.1, "confidence": 0.3},
        )

    scenario(
        "a password field with no stored secret stops needs_login",
        "Log in to acme.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": signin},
            "urls": {"https://acme.test/login": "login"},
        },
        {"Plan": [plan(("a1", "Log in", "action"), answer=False)]},
        start="https://acme.test/login",
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
    )

    scenario(
        "a password field no stored secret may reach stops needs_login",
        "Log in to acme.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "login": signin},
            "urls": {"https://acme.test/login": "login"},
        },
        {"Plan": [plan(("a1", "Log in", "action"), answer=False)]},
        start="https://acme.test/login",
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
        secrets={"vault_password": ["kept", ["https://shop.test"]]},
    )

    scenario(
        "a download the task asks for is dispatched after one question, and the step limit ends the run",
        "Download version 2.0 from files.test",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": downloads},
            "urls": {"https://files.test/": "home"},
            "actions": [
                {"screen": "home", "operation": "click", "result": {"outcome": "executed", "page_changed": False}}
            ],
        },
        {"Plan": [plan(("a1", "Download version 2.0", "action"), answer=False)]},
        start="https://files.test/",
        jev={"pick": {"operation": "click"}, "noul": 0.95, "nouls": {"downloads": 0.05, "irreversible": 0.0}},
        limits={"max_steps": 1},
    )

    scenario(
        "a dollar budget ends the run budget_exceeded",
        "Search books.test for dune and tell me who wrote it",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "home": search},
            "urls": {"https://books.test/": "home"},
        },
        {
            "Plan": [plan(("r1", "Find who wrote Dune", "information"), checks=["the author of Dune"])],
            "_FieldText": [{"missing": False, "text": "dune"}],
        },
        start="https://books.test/",
        jev={"pick": {"operation": "fill"}, "noul": 0.1},
        limits={"max_dollars": 0.0025},
    )

    alert = screen(
        "https://mail.test/inbox",
        "Inbox",
        "Inbox\n\nYou have new mail.",
        [control("open", "link", "Open", "click", href="/m/1")],
        dialog={"kind": "alert", "message": "Session will expire soon"},
    )
    scenario(
        "dismissing an alert is not gated, and the step limit ends the run",
        "Open my newest mail on https://mail.test/inbox",
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), "inbox": alert},
            "urls": {"https://mail.test/inbox": "inbox"},
        },
        {"Plan": [plan(("a1", "Open the newest mail", "action"), answer=False)]},
        start="https://mail.test/inbox",
        jev={"pick": {"operation": "dialog", "pick": "dismiss"}, "noul": 0.1},
        limits={"max_steps": 1},
    )
    filler = " Quoted context." * 450
    replies_text = (
        f"Replies\n\nProject 0: Author says the fix is available.{filler}\n\n"
        f"Project 1: Author says the fix is planned.{filler}"
    )
    scenario(
        "a page whose text alone overflows Jev's input stops observation_limit",
        "Report the reply for Project 0 from https://projects.test/replies",
        {
            "start": "blank",
            "screens": {
                "blank": screen("about:blank", "", ""),
                "replies": screen("https://projects.test/replies", "Replies", replies_text),
            },
            "urls": {"https://projects.test/replies": "replies"},
        },
        {
            "Plan": [plan(("r0", "Report the reply for Project 0", "information"), checks=["the reply for Project 0"])],
        },
        start="https://projects.test/replies",
        config={"tokens": {"state_plus_largest_question": 2500, "state_plus_all_questions": 5000}},
        jev={"pick": {"operation": "read"}, "noul": 0.1},
    )

    projects = ("available", "planned", "released")

    def project(number: int) -> dict[str, Any]:
        url = f"https://projects.test/project-{number}"
        lead = f"Project {number}: Author says the fix is {projects[number]}."
        return screen(url, f"Project {number}", f"Project {number}\n\n{lead}{filler}", viewport_text=lead)

    def reply(number: int) -> dict[str, Any]:
        text = f"Project {number}: Author says the fix is {projects[number]}."
        claim = {"requirement_id": f"r{number}", "text": text, "cite": {"first": "s1", "last": "s1"}}
        return {"claims": [claim], "answered": number == len(projects) - 1}

    numbers = range(len(projects))
    scenario(
        "notes too large for one finish prompt are partitioned per project and each part completes",
        "Report the reply for each project: "
        + ", ".join(f"Project {n} at https://projects.test/project-{n}" for n in numbers),
        {
            "start": "blank",
            "screens": {"blank": screen("about:blank", "", ""), **{f"p{n}": project(n) for n in numbers}},
            "urls": {f"https://projects.test/project-{n}": f"p{n}" for n in numbers},
        },
        {
            "Plan": [
                plan(
                    *((f"r{n}", f"Report the reply for Project {n}", "information") for n in numbers),
                    checks=[f"the reply for Project {n}" for n in numbers],
                )
            ],
            "_FinishOutputs": [
                {
                    "groups": [
                        {
                            "subjects": [{"name": f"Project {n}", "requirement_ids": [f"r{n}"]} for n in numbers],
                            "fields": ["the reply"],
                            "check_indices": list(numbers),
                        }
                    ]
                }
            ],
            "_ReadResponse": [reply(n) for n in numbers],
            "_FieldAnswerDraft": [
                {"output_0": [{"text": f"Project {n}: Author says the fix is {projects[n]}.", "evidence_ids": ["e0"]}]}
                for n in numbers
            ],
            "LLMVerdict": [{"missing": [], "complete": True}],
        },
        start="https://projects.test/project-0",
        config={"tokens": {"state_plus_largest_question": 8800, "state_plus_all_questions": 17600}},
        jev={
            "pick": {
                "operation": ["read", "navigate", "read", "navigate", "read", "done"],
                "navigate_target": ["0", "0", "1", "1"],
            },
            "noul": 0.95,
            "nouls": {
                "draft_needs_writing": 0.0,
                "destination": 0.95,
                "irreversible": 0.0,
                "unsupported_0": 0.0,
                "contradicted_0": 0.0,
            },
        },
        limits={"max_steps": 10},
    )
    return found


def run_scenario(spec: Mapping[str, Any]) -> dict[str, Any]:
    calls: list[dict[str, Any]] = []
    jev_spec = spec.get("jev", {})
    jev = RecordingJev(calls, jev_spec.get("pick", {}), jev_spec.get("noul", 0.8), jev_spec.get("nouls", {}))
    confidence = jev_spec.get("confidence")
    if confidence is not None:
        original = jev.evaluate

        async def unsure(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            evaluation = await original(state, questions)
            answers = {
                key: answer.model_copy(update={"confidence": confidence})
                if isinstance(answer, ChoiceAnswer)
                else answer
                for key, answer in evaluation.answers.items()
            }
            changed = evaluation.model_copy(update={"answers": answers})
            calls[-1]["response"] = {"evaluation": changed.model_dump(mode="json")}
            return changed

        jev.evaluate = unsure  # ty: ignore[invalid-assignment]
    llm = RecordingLLM(calls, spec["llm"])
    secrets = None
    if spec.get("secrets"):
        secrets = ScopedSecrets.per_secret(
            {name: (value, tuple(origins)) for name, (value, origins) in spec["secrets"].items()}
        )
    events: list[dict[str, Any]] = []

    async def on_event(event: Any) -> None:
        if isinstance(event, StepEvent):
            step = event.step.model_dump(mode="json")
            step["duration_ms"] = 0
            events.append({"step": step, "frame": event.frame is not None})

    async def go() -> Any:
        page = FakeSite(spec["site"])
        config = Config.model_validate({**spec.get("config", {}), "step_frames": spec.get("step_frames", False)})
        agent = Agent(page, jev, llm, config=config, secrets=secrets, on_event=on_event)
        accepted = spec.get("until")

        async def until(url: str) -> bool:
            return url in (accepted or ())

        return await agent.run(
            spec["task"],
            until=until if accepted is not None else None,
            start=spec.get("start"),
            choose_start=spec.get("choose_start", False),
            inputs=spec.get("inputs"),
            attachments=[Attachment(**{**a, "content": a["content"].encode()}) for a in spec.get("attachments", ())],
            limits=Limits.model_validate(spec["limits"]) if spec.get("limits") else None,
            authorization=Authorization.model_validate(spec["authorization"]) if spec.get("authorization") else None,
        )

    if spec.get("interstitial"):
        # Wait for a page to move on, as the scenario's settling screen does.
        agent_module._INTERSTITIAL_SECONDS = 1.0
    try:
        result = asyncio.run(go())
    finally:
        agent_module._INTERSTITIAL_SECONDS = 0.0
    outcome = result.model_dump(mode="json")
    for step in outcome["steps"]:
        step["duration_ms"] = 0
    return {**spec, "calls": calls, "outcome": outcome, "events": events}


def main() -> None:
    cases = []
    for spec in scenarios():
        try:
            cases.append(run_scenario(spec))
        except Exception:
            print(f"FAILED: {spec['name']}")
            raise
    for case in cases:
        print(f"{case['outcome']['status']:20} {len(case['calls']):3} calls  {case['name']}")
    (HERE / "agent.json").write_text(json.dumps(cases, indent=1, ensure_ascii=True) + "\n", encoding="ascii")


if __name__ == "__main__":
    main()
