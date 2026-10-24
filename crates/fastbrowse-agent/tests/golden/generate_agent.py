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
from fastbrowse.config import Config, StallRules
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation, Message
from fastbrowse.models import (
    Authorization,
    CostBasis,
    CostComponent,
    CostLine,
    Limits,
    LLMPurpose,
    Operation,
    SecretRef,
    StepEvent,
    StepOutcome,
)
from fastbrowse.page import Action, ActResult, BrowserError, Capture, Control, Observation
from fastbrowse.safety import ScopedSecrets
from fastbrowse.telemetry import Ledger

HERE = Path(__file__).parent


def scripted_identities(messages: Sequence[Message]) -> JsonValue:
    """tests/test_retrieval.py's answer for the identity audit: every criterion bound to the first quoted source."""
    payload = json.loads(messages[-1].content)
    reference, source = next(iter(payload["sources"].items()))
    return {
        "identities": [{"id": "i0", "source_ref": reference, "quote": source["quote"]}],
        "bindings": {key: {"scope": "entities", "identity_ids": ["i0"]} for key in payload["criteria"]},
    }


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

    def _screen(self) -> Mapping[str, Any]:
        return self.site["screens"][self.screen]

    @property
    def artifacts(self) -> tuple[()]:
        return ()

    async def observe(self) -> Observation:
        return Observation.model_validate(self._screen()["observation"])

    async def document_changed(self, observation: Observation) -> bool:
        return False

    async def navigate(self, url: str, *, back_to: str | None = None) -> None:
        screen = self.site["urls"].get(url)
        if screen is None:
            raise BrowserError(f"no page at {url}")
        self.back.append(self.screen)
        self.screen = screen

    async def capture(self) -> Capture:
        return Capture.model_validate(self._screen()["capture"])

    async def act(self, action: Action, observation: Observation) -> ActResult:
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
        if schema.__name__ == "_OutputIdentities":
            queue = [scripted_identities(messages)]
        elif schema.__name__ == "_OutputAssessment" and not queue:
            # Every audited output judged supported by its quotes.
            keys = schema.model_json_schema()["$defs"]["_RequiredJudgments"]["properties"]
            queue = [{"judgments": dict.fromkeys(keys, "yes"), "reason": "The quotes state it."}]
        if not queue:
            raise AssertionError(f"no scripted {schema.__name__} for {purpose.value}")
        data = queue.pop(0) if len(queue) > 1 else queue[0]
        cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
        self.calls.append(
            {
                "llm": {
                    "purpose": purpose.value,
                    "messages": compact([message.model_dump(mode="json") for message in messages]),
                    "schema_name": schema.__name__,
                    "schema": json.dumps(schema.model_json_schema()),
                    "max_output_tokens": max_output_tokens,
                },
                "response": {"data": data, "cost": cost.model_dump(mode="json")},
            }
        )
        return Generation(data=schema.model_validate(data), cost=cost)


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
            "Plan": [plan(("r1", "Find the last order total", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the order total"]}],
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
            "Plan": [plan(("r1", "Find the blue mug's price", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the price"]}],
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
            "Plan": [plan(("r1", "Find who wrote Dune", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the author of Dune"]}],
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
            "Plan": [plan(("r1", "Find who wrote Dune", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the author of Dune"]}],
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
            "Plan": [plan(("r1", "Tell something interesting", "information"))],
            "StartPage": [{"url": None}],
            "_AnswerChecks": [{"checks": ["something"]}],
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
            "Plan": [plan(("r1", "Find the returns policy", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the returns policy"]}],
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
            "Plan": [plan(("r1", "Find the latest version", "information"))],
            "Shortcut": [{"url": None}],
            "_AnswerChecks": [{"checks": ["the latest version"]}],
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
            "Plan": [plan(("r1", "Find the latest httpx version", "information"))],
            "Shortcut": [{"url": "https://pypi.test/project/httpx/"}],
            "_AnswerChecks": [{"checks": ["the latest httpx version"]}],
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
            "_Recovery": [recovery("Unsure which control", "Select business class", control=0, operation="select")],
        },
        start="https://travel.test/",
        jev={"pick": {"operation": "select", "pick": "1"}, "noul": 0.1, "confidence": 0.3},
        limits={"max_steps": 1},
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
            events.append(step)

    async def go() -> Any:
        page = FakeSite(spec["site"])
        agent = Agent(page, jev, llm, config=Config(stall=StallRules()), secrets=secrets, on_event=on_event)
        return await agent.run(
            spec["task"],
            start=spec.get("start"),
            choose_start=spec.get("choose_start", False),
            limits=Limits.model_validate(spec["limits"]) if spec.get("limits") else None,
            authorization=Authorization.model_validate(spec["authorization"]) if spec.get("authorization") else None,
        )

    result = asyncio.run(go())
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

_ = SecretRef
