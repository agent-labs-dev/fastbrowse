"""Write the bytes Python produces for the Rust core to match.

    uv run python crates/fastbrowse-core/tests/golden/generate.py

Each file holds cases whose expected output is a string of exactly what Python wrote, so the Rust tests in
`tests/golden.rs` compare bytes, not parsed values. The files themselves are written with `ensure_ascii`, so they stay
plain ASCII whatever the strings inside hold.

- `models.json`: model instances as `model_dump_json()`, `json.dumps(model_dump(mode="json"))` and the same with
  `exclude_none=True`, which Rust reads back from the first and writes again three ways.
- `schemas.json`: `model_json_schema()` and its strict form, for the models whose schemas cross a seam.
- `functions.json`: what the ported functions return for given inputs.
"""

import json
from datetime import UTC, datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from fastbrowse.clients.openai_compatible import strict_schema
from fastbrowse.config import Config, StallRules, Thresholds, TokenBudget
from fastbrowse.effects import effect, state_key, step_key
from fastbrowse.jev import (
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    NoulAnswer,
    NoulQuestion,
    ScoreAnswer,
    ScoreQuestion,
)
from fastbrowse.llm import Message
from fastbrowse.models import (
    Artifact,
    ArtifactKind,
    Attachment,
    Authorization,
    BrowserConnection,
    BrowserEvent,
    BudgetStop,
    Citation,
    CostBasis,
    CostBreakdown,
    CostComponent,
    CostLine,
    Decider,
    Evidence,
    FactReader,
    Limits,
    LLMPurpose,
    LocalChrome,
    Operation,
    RunResult,
    SecretRef,
    SourceControl,
    Status,
    StepEvent,
    StepFact,
    StepOutcome,
    StepResult,
    Tripwire,
    TripwireMode,
    Wall,
    WallKind,
)
from fastbrowse.navigation import navigation_urls, task_urls
from fastbrowse.output_schema import output_model
from fastbrowse.page import (
    Action,
    ActResult,
    Block,
    BlockKind,
    Capture,
    Control,
    Dialog,
    Observation,
    Tab,
    cut_text,
)
from fastbrowse.protocol import (
    Error,
    ErrorCode,
    ErrorResponse,
    InitializeResult,
    Notification,
    Request,
    Response,
    RunCancelParams,
    RunEvent,
    RunFrame,
    RunParams,
    RunUntilParams,
    SecretsResolveParams,
    ServerMethod,
    ServerRequest,
)
from fastbrowse.safety import Redactor, downloads_question, irreversible_question
from fastbrowse.safety import origin_of as safety_origin_of
from fastbrowse.telemetry import _dollars

HERE = Path(__file__).parent
WHEN = datetime(2026, 9, 27, 10, 30, 5, 120000, tzinfo=UTC)
TRICKY = 'caf\u00e9 \u6771\u4eac "q" \\ \n\t \u2028 \U0001f600 \x01 \u2019'


def control(id: str, label: str, role: str = "button", *operations: Operation, **values: Any) -> Control:
    values.setdefault("frame_id", None)
    return Control(id=id, role=role, label=label, operations=frozenset(operations), **values)


FIELD = control(
    "f1",
    "Where from?",
    "combobox",
    Operation.FILL,
    frame_id="frame-1",
    frame_origin="https://example.test",
    form_id="search",
    retarget_key="guard",
    context="Flights \u203a Outbound",
    value="Lond",
    options=("London", "Londonderry"),
    input_type="text",
    input_name="from",
    autocomplete="off",
    expanded=True,
    blocking=True,
)
LINK = control("l1", "Next \u203a", "link", Operation.CLICK, href="/page/2", next_page=True, offscreen=True)
BOX = control("c1", "Direct only", "checkbox", Operation.CLICK, checked=False, sensitive=False)

OBSERVATION = Observation(
    url="https://example.test/search?q=kettle#top",
    title=TRICKY,
    page_key="k1",
    captured_at=WHEN,
    response_status=200,
    document_key="doc-1",
    controls=(FIELD, LINK, BOX),
    omitted_controls=3,
    viewport_text="Viewport \u00a310.69\nline two",
    tabs=(
        Tab(id="t1", url="https://example.test/", title="Example", active=True, opener_id=None),
        Tab(id="t2", url="about:blank", title="", active=False, opener_id="t1"),
    ),
    dialog=Dialog(kind="prompt", message="Name?", default_prompt="Ada"),
    inaccessible_frames=1,
    can_go_back=True,
    history_entry="h3",
)

CAPTURE = Capture(
    url="https://example.test/item",
    title="Item",
    captured_at=WHEN,
    sha256="0" * 64,
    text="Kettle\nPrice \u00a310.69\nIn stock",
    blocks=(
        Block(source_id="b1", kind=BlockKind.HEADING, frame_id=None, start=0, end=6, heading_path=()),
        Block(
            source_id="b2",
            kind=BlockKind.PARAGRAPH,
            frame_id="frame-1",
            start=7,
            end=19,
            heading_path=("Kettle",),
            control_context=SourceControl(role="row", label="Price", context=None),
            href="https://example.test/price",
            list_id="l",
            list_count=2,
            list_frameless=True,
        ),
        Block(source_id="b3", kind=BlockKind.OBSERVATION, frame_id=None, start=20, end=28, complete_source=True),
    ),
    inaccessible_frames=0,
)

EVIDENCE = Evidence(
    source_id="b2",
    url="https://example.test/item",
    frame_id=None,
    captured_at=datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone(timedelta(hours=5, minutes=30))),
    capture_sha256="ab" * 32,
    start=7,
    end=19,
    quote="Price \u00a310.69",
    control_context=SourceControl(role="row", label="Price", context="Kettle"),
    heading_path=("Kettle", "Offers"),
)

LINES = (
    CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.000042, input_tokens=1000, seconds=0.5),
    CostLine(
        component=CostComponent.LLM,
        basis=CostBasis.ESTIMATED,
        dollars=0.1 + 0.2,
        purpose=LLMPurpose.READ,
        input_tokens=12000,
        output_tokens=800,
        seconds=3.25,
    ),
    CostLine(component=CostComponent.LLM, basis=CostBasis.BOUNDED, dollars=1e-05, purpose=LLMPurpose.PLAN),
    CostLine(component=CostComponent.BROWSER, basis=CostBasis.UNKNOWN),
    CostLine(component=CostComponent.PROXY, basis=CostBasis.METERED, dollars=12.0, seconds=1e16),
)

FACT = StepFact(
    text="The kettle costs \u00a310.69",
    requirement_id="r1",
    quote="\u00a310.69",
    url="https://example.test/item",
    reader=FactReader.LLM,
    deep_link="https://example.test/item#:~:text=%C2%A310.69",
)
STEP = StepResult(
    index=3,
    operation=Operation.CLICK,
    decided_by=Decider.JEV,
    outcome=StepOutcome.EXECUTED,
    url="https://example.test/",
    target="Next \u203a",
    confidence=0.875,
    note=None,
    facts=(FACT, StepFact(text="3 results", reader=FactReader.JEV_CHOICE)),
    page_changed=True,
    duration_ms=412,
)

RESULT = RunResult(
    status=Status.BUDGET_EXCEEDED,
    budget=BudgetStop(resource="jev_calls", limit=40),
    answer="It costs \u00a310.69 [1].",
    data={"price": 10.69, "items": [1, 2.0, None, True, "\u6771"], "nested": {"empty": {}, "list": []}, "big": 1e300},
    evidence=(EVIDENCE,),
    steps=(STEP,),
    cost=CostBreakdown(lines=LINES),
    artifacts=(
        Artifact(
            kind=ArtifactKind.DOWNLOAD,
            name="report.csv",
            mime_type="text/csv",
            size_bytes=4,
            sha256="cd" * 32,
            uri="file:///tmp/download/abc/report.csv",
        ),
    ),
    citations=(
        Citation(
            id=1,
            text="The kettle costs \u00a310.69",
            url="https://example.test/item",
            quote="\u00a310.69",
            deep_link="https://example.test/item#:~:text=x",
        ),
    ),
    error="time limit 60.0s reached",
    final_url="https://example.test/item",
    final_frame=b"\x89PNG\r\n\x1a\n",
    would_fire=(Tripwire.NO_PROGRESS, Tripwire.NO_PROGRESS, Tripwire.PLAN_STAGNATION),
    recordings=(Path("/tmp/run/captioned.mp4"), Path("/tmp/run/plain.mp4")),
    wall=Wall(kind=WallKind.ONE_TIME_CODE, origin="https://example.test", fields=("Code",), seen_at_ms=1793000000000),
)

QUESTIONS = {
    "choice": ChoiceQuestion(
        instructions=TRICKY,
        criteria={
            "0": "Kettle",
            "1": {"what": "A mug", "not_for": ["tea"], "examples": None},
            "skip": None,
            "n": 1.5,
        },
    ),
    "noul": NoulQuestion(instructions="Is it done?", true="Yes", false=None),
    "score": ScoreQuestion(instructions="How good?", criteria=("bad", "fine", "good")),
}

EVALUATION = Evaluation(
    model="jev-1.13.0",
    answers={
        "operation": ChoiceAnswer(choice="click", probabilities={"click": 0.9, "fill": 0.1}, confidence=0.8),
        "login_required": NoulAnswer(probability=0.2),
        "quality": ScoreAnswer(score=1.3, probabilities={"bad": 0.1, "fine": 0.5, "good": 0.4}, confidence=0.6),
    },
    input_tokens=1234,
    cost=LINES[0],
    requests=2,
)


def models() -> list[dict[str, Any]]:
    cases: list[tuple[str, BaseModel]] = [
        ("SourceControl", SourceControl(role="row", label=TRICKY)),
        ("Evidence", EVIDENCE),
        ("Citation", RESULT.citations[0]),
        ("StepFact", FACT),
        ("Artifact", RESULT.artifacts[0]),
        *(("CostLine", line) for line in LINES),
        ("CostBreakdown", CostBreakdown(lines=LINES)),
        ("StepResult", STEP),
        ("BudgetStop", BudgetStop(resource="dollars", limit=0.25)),
        ("RunResult", RESULT),
        (
            "RunResult",
            RunResult(
                status=Status.COMPLETE,
                answer=None,
                data=None,
                evidence=(),
                steps=(),
                cost=CostBreakdown(),
                artifacts=(),
            ),
        ),
        (
            "BrowserConnection",
            BrowserConnection(
                cdp_url="ws://127.0.0.1:9222/devtools/browser/x",
                remote=False,
                foreground=True,
                allowed_origins=("https://A.example", "https://b.example:8443/"),
            ),
        ),
        ("LocalChrome", LocalChrome(binary="chromium", headed=True, profile=Path("/tmp/profile"))),
        ("Limits", Limits(max_steps=40, max_jev_calls=100, max_dollars=0.5, max_seconds=300)),
        ("Limits", Limits()),
        ("Authorization", Authorization(irreversible_actions=True)),
        ("SecretRef", SecretRef(name="password", origins=("https://*.example.test", "https://login.test"))),
        ("StepEvent", StepEvent(step=STEP, frame=b"\x89PNG\r\n\x1a\n\x00\xff")),
        ("StepEvent", StepEvent(step=STEP)),
        ("BrowserEvent", BrowserEvent(live_url="https://live.example/x", browser_id="b-1")),
        ("BrowserEvent", BrowserEvent(live_url=None)),
        ("Control", FIELD),
        ("Control", LINK),
        ("Control", BOX),
        ("Tab", OBSERVATION.tabs[1]),
        ("Dialog", OBSERVATION.dialog),
        ("Observation", OBSERVATION),
        ("Block", CAPTURE.blocks[1]),
        ("Capture", CAPTURE),
        (
            "Action",
            Action(
                operation=Operation.UPLOAD,
                target_id="f1",
                files=(Attachment(name="a.txt", mime_type="text/plain", content=b"hello \xc3\xa9"),),
                accept_dialog=False,
                form_fill=True,
            ),
        ),
        ("ActResult", ActResult(outcome=StepOutcome.STALE, page_changed=False, detail="redrawn", moved=True)),
        ("Config", Config()),
        (
            "Config",
            Config(
                thresholds=Thresholds(recover_below=0.6),
                tokens=TokenBudget(chars_per_token=2.5),
                stall=StallRules(tripwires=TripwireMode.ARMED),
                step_frames=True,
                max_pages=0,
            ),
        ),
        *(("Question", question) for question in QUESTIONS.values()),
        ("Evaluation", EVALUATION),
        ("Message", Message(role="system", content=TRICKY)),
        ("Request", Request(id=7, method="run", params={"run_id": "r", "task": "t"})),
        ("Request", Request(method="shutdown")),
        ("Response", Response(id="a", result=RESULT.model_dump(mode="json"))),
        ("Response", Response(id=1, result=None)),
        (
            "ErrorResponse",
            ErrorResponse(
                id=None, error=Error(code=ErrorCode.PARSE_ERROR, message="Expecting value: line 1 column 1 (char 0)")
            ),
        ),
        ("InitializeResult", InitializeResult(protocol_version=1, fastbrowse_version="0.5.25")),
        (
            "RunParams",
            RunParams.model_validate(
                {
                    "run_id": "run-1",
                    "task": TRICKY,
                    "start": "https://example.test/",
                    "inputs": {"name": "Ada", "city": "\u6771\u4eac"},
                    "attachments": [{"name": "a.txt", "mime_type": "text/plain", "content": "aGVsbG8="}],
                    "limits": {"max_steps": 10, "max_dollars": 1},
                    "authorization": {"irreversible_actions": False},
                    "output_schema": {"type": "object", "properties": {"price": {"type": "number"}}},
                    "secrets": [{"name": "password", "origins": ["https://example.test"]}],
                    "frames": True,
                    "until": True,
                    "downloads": "/tmp/downloads",
                    "chrome": {"headed": True},
                    "cdp_port": 9222,
                    "viewport": [1280, 720],
                }
            ),
        ),
        ("RunParams", RunParams(run_id="r", task="t")),
        ("RunCancelParams", RunCancelParams(run_id="r")),
        ("RunEvent", RunEvent(run_id="r", event=StepEvent(step=STEP))),
        ("RunEvent", RunEvent(run_id="r", event=BrowserEvent(live_url=None, browser_id="b"))),
        ("RunFrame", RunFrame(run_id="r", frame=b"\xff\xd8\xff\xe0" * 5)),
        ("Notification", Notification(method=ServerMethod.RUN_EVENT, params={"run_id": "r", "x": [1.5, None]})),
        (
            "ServerRequest",
            ServerRequest(
                id=3,
                method=ServerMethod.SECRETS_RESOLVE,
                params={"run_id": "r", "name": "p", "origin": "https://example.test"},
            ),
        ),
        ("SecretsResolveParams", SecretsResolveParams(run_id="r", name="p", origin="https://example.test")),
        ("RunUntilParams", RunUntilParams(run_id="r", url="https://example.test/done")),
    ]
    written = []
    for name, model in cases:
        dumped = model.model_dump(mode="json")
        written.append(
            {
                "type": name,
                "dump_json": model.model_dump_json(),
                "dumps": json.dumps(dumped),
                "exclude_none": json.dumps(model.model_dump(mode="json", exclude_none=True)),
                "httpx": json.dumps(dumped, ensure_ascii=False, separators=(",", ":")),
            }
        )
    # A frozenset is written in hash order, which changes from one process to the next; Rust writes it sorted, as
    # `sorted(control.operations)` orders it, so the expected bytes here are pydantic's with the list sorted.
    several = control("s1", "Search", "searchbox", Operation.FILL, Operation.ENTER, Operation.CLICK, Operation.SELECT)
    dumped = several.model_dump(mode="json")
    dumped["operations"] = sorted(dumped["operations"])
    written.append(
        {
            "type": "Control",
            "dump_json": json.dumps(dumped, ensure_ascii=False, separators=(",", ":")),
            "dumps": json.dumps(dumped),
            "exclude_none": json.dumps({k: v for k, v in dumped.items() if v is not None}),
            "httpx": json.dumps(dumped, ensure_ascii=False, separators=(",", ":")),
        }
    )
    return written


OUTPUT_SCHEMAS: dict[str, dict[str, Any]] = {
    "order": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "customer": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "vip": {"type": "boolean"}},
                "required": ["name", "vip"],
            },
            "lines": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"sku": {"type": "string"}, "quantity": {"type": "integer"}},
                    "required": ["sku", "quantity"],
                },
            },
        },
        "required": ["id", "customer", "lines"],
    },
    "zod": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "The product name"},
            "count": {"type": "integer", "minimum": -9007199254740991, "maximum": 9007199254740991},
            "rating": {"type": ["number", "null"]},
            "size": {"type": "string", "enum": ["small", "large"]},
            "currency": {"default": "EUR", "type": "string"},
            "seller": {"$ref": "#/$defs/Seller"},
        },
        "required": ["name", "count", "rating", "size", "seller"],
        "title": "Product",
        "$defs": {"Seller": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
    },
    "references": {
        "type": "object",
        "properties": {
            "billing": {"$ref": "#/$defs/address"},
            "shipping": {"$ref": "#/$defs/address", "description": "Where it goes"},
            "history": {"type": "array", "items": {"$ref": "#/$defs/address"}},
        },
        "required": ["billing", "shipping", "history"],
        "$defs": {
            "address": {
                "type": "object",
                "properties": {"city": {"type": "string"}, "country": {"$ref": "#/$defs/country"}},
                "required": ["city", "country"],
            },
            "country": {"enum": ["nl", "de"]},
        },
    },
    "twins": {
        "type": "object",
        "properties": {
            "a": {
                "type": "object",
                "properties": {"item": {"type": "object", "properties": {"n": {"type": "string"}}}},
            },
            "b": {
                "type": "object",
                "properties": {"item": {"type": "object", "properties": {"m": {"type": "integer"}}}},
            },
            "c": {"type": "array", "items": {"type": "object", "properties": {"n": {"type": "string"}}}},
        },
    },
    "unions": {
        "type": "object",
        "properties": {"first": {"type": ["number", "integer"]}, "second": {"type": ["integer", "number"]}},
        "required": ["first", "second"],
    },
    "every_type": {
        "type": "object",
        "properties": {
            "order-id": {"type": "string", "description": "d"},
            "class": {"type": "integer", "minimum": 0},
            "x": {"type": "object", "properties": {"a": {"type": "number"}}, "additionalProperties": False},
            "e": {"enum": ["small", 3, None]},
            "f": {"enum": [1.5]},
            "g": {"type": ["null", "string"]},
            "h": {"enum": [True]},
            "i": {"anyOf": [{"type": "null"}, {}]},
            "arr": {"type": "array", "items": {"type": "object", "properties": {"z": {"type": "string"}}}},
            "j": {"type": ["integer", "number", "null"]},
            "k": {"type": "array"},
            "l": {"const": "kettle"},
            "m": {"type": "integer", "enum": [1, 2]},
            "na\u00efve": {"type": "boolean"},
            "copy": {"type": "string"},
            "2fa": {"type": "string"},
            "default": {"type": "object", "properties": {"title": {"type": "string"}, "a": {"type": "string"}}},
        },
        "required": ["order-id"],
    },
}


def schemas() -> list[dict[str, Any]]:
    written = []
    for name, model in [
        ("SourceControl", SourceControl),
        ("Evidence", Evidence),
        ("CostLine", CostLine),
        ("BudgetStop", BudgetStop),
        ("StepFact", StepFact),
        ("StepResult", StepResult),
        ("StepEvent", StepEvent),
        ("Limits", Limits),
        ("Evaluation", Evaluation),
    ]:
        schema = model.model_json_schema()
        written.append({"type": name, "schema": json.dumps(schema), "strict": strict_text(schema)})
    for name, source in OUTPUT_SCHEMAS.items():
        schema = output_model(source).model_json_schema()
        written.append({"output": source, "type": name, "schema": json.dumps(schema), "strict": strict_text(schema)})
    return written


def strict_text(schema: Any) -> str | None:
    try:
        return json.dumps(strict_schema(schema))
    except ValueError as error:
        return f"error: {error}"


FLOATS = [
    0.0,
    -0.0,
    1.0,
    0.1,
    0.2,
    0.1 + 0.2,
    1 / 3,
    2 / 3,
    1e-4,
    1e-5,
    1.5e-5,
    1e-7,
    123e-9,
    1e15,
    1e16,
    1.5e16,
    123456789012345678.0,
    1e22,
    1e300,
    5e-324,
    2.2250738585072014e-308,
    1.7976931348623157e308,
    12.5,
    0.000042,
    9007199254740993.0,
    3.14159,
    -2.5e-6,
    100.0,
    1234.5678,
]
STRINGS = [
    TRICKY,
    "",
    "plain",
    "\x7f\x00\x1f",
    "\u00ad\u200b\ufeff",
    "it's",
    'say "hi"',
    "both ' and \"",
    "\U0010fffd\ue000",
]


def functions() -> dict[str, Any]:
    redactor = Redactor()
    redactor.register("username", "practice", "https://practice.expandtesting.com/login")
    redactor.register("password", 'p@ss w"ord\u00e9')
    redactable = [
        "https://practice.expandtesting.com/?u=practice&p=p%40ss%20w%22ord%C3%A9",
        'log: {"password": "p@ss w\\"ord\\u00e9"} and p@ss+w%22ord%C3%A9',
        "see https://practice.expandtesting.com. then practice \u6771\u4eac practice",
    ]
    target = control(
        "b", "Yes, I'm sure", "button", Operation.CLICK, context='Delete "all" \u200b', submit_semantics="POST /delete"
    )
    destination = control("d", "Trash", "listitem", Operation.CLICK, context="Sidebar")
    before = OBSERVATION.model_copy(update={"document_key": "doc"})
    after = OBSERVATION.model_copy(
        update={
            "url": "https://example.test/next",
            "response_status": 404,
            "controls": (
                FIELD.model_copy(update={"value": "London", "expanded": False}),
                BOX.model_copy(update={"checked": True}),
                control("n1", "x" * 70, "button"),
            ),
        }
    )
    tasks = [
        ("Visit https://alpha.example.test/project, then https://beta.example.test/p?tab=1#top.", None),
        ("Compare **https://a.example.test/x** and __https://b.example.test/y__;https://c.example.test/z", None),
        ("Read [docs](https://docs.example.test/a_(b)) and HTTPS://EXAMPLE.TEST:443/", None),
        ("Sign in at /login as a@example.test, then open `/account/orders?page=2`.", "https://example.test/entry"),
        ("Open it 24/7 and/or read (/help) and **/faq**!", "http://shop.example/entry"),
    ]
    return {
        "floats": [
            {
                "value": value,
                "repr": json.dumps(value),
                "pydantic": CostLine(component="jev", basis="metered", dollars=abs(value)).model_dump_json(),
            }
            for value in FLOATS
        ],
        "strings": [
            {
                "value": value,
                "dumps": json.dumps(value),
                "unicode": json.dumps(value, ensure_ascii=False),
                "repr": repr(value),
                "pydantic": SourceControl(role="r", label=value).model_dump_json(),
            }
            for value in STRINGS
        ],
        "dollars": [
            {"value": value, "shown": _dollars(value)}
            for value in [0.00001, 0.25, 1.0, 12.5, 1e16, 1.5e-7, 1e-05, 123456.789]
        ],
        "state_key": state_key(OBSERVATION),
        "step_key": step_key(OBSERVATION, CAPTURE),
        "read_key": CAPTURE.read_key,
        "today": [
            OBSERVATION.model_copy(update={"captured_at": when}).today
            for when in [WHEN, datetime(2026, 1, 31), datetime(2024, 2, 29, 23, 59), datetime(2026, 12, 15, tzinfo=UTC)]
        ],
        "cut_text": [
            {"text": text, "max": limit, "json": encoded, "cut": cut_text(text, limit, json_encoded=encoded)}
            for text, limit, encoded in [
                ("a" * 200, 100, False),
                ("\u6771\u4eac" * 300, 1000, True),
                ('quote " and \n newline ' * 30, 300, True),
                ("short", 10, False),
                ("x" * 80, 70, False),
            ]
        ],
        "questions": {
            "irreversible": irreversible_question(
                "Tidy up \u00e9", Operation.CLICK, target, destination
            ).model_dump_json(),
            "enter": irreversible_question("Send it", Operation.ENTER, target).model_dump_json(),
            "link": irreversible_question("Go on", Operation.CLICK, LINK).model_dump_json(),
            "link_enter": irreversible_question("Go on", Operation.ENTER, LINK).model_dump_json(),
            "downloads": downloads_question("Read the report, do not save anything").model_dump_json(),
        },
        "redact": [
            {"text": text, "redacted": redactor.redact(text), "masked": redactor.mask(text)} for text in redactable
        ],
        "effect": effect(before, after, FIELD).summary,
        "task_urls": [
            {
                "task": task,
                "start": start,
                "urls": list(task_urls(task, start=start)),
                "navigation": list(navigation_urls(task, "https://example.test/", start=start)),
            }
            for task, start in tasks
        ],
        "safety_origin": [
            {"url": url, "origin": safety_origin_of(url)}
            for url in [
                "https://Shop.Example.test:443/cart",
                "http://x.test:8080/",
                "https://user:pw@[::1]:8443/",
                "https://a.example.test.",
            ]
        ],
        "validate": [
            {
                "schema": name,
                "data": data,
                "dumped": json.dumps(output_model(OUTPUT_SCHEMAS[name]).model_validate(data).model_dump(mode="json")),
            }
            for name, data in [
                (
                    "order",
                    {"id": "A", "customer": {"name": "Ada", "vip": True}, "lines": [{"sku": "k", "quantity": 2.0}]},
                ),
                (
                    "zod",
                    {"name": "K", "count": 3, "rating": 4, "size": "large", "seller": {"name": "S"}, "currency": "EUR"},
                ),
                (
                    "every_type",
                    {
                        "order_id": "x",
                        "class": 1,
                        "e": None,
                        "f": 1.5,
                        "j": 2.0,
                        "l": "kettle",
                        "k": [1, "a"],
                        "copy": "c",
                        "x": {"a": 1},
                        "arr": [{}],
                        "default": {"title": "t"},
                    },
                ),
                *(("unions", {"first": value, "second": value}) for value in [3, 2.0, 3.5, -0.0]),
            ]
        ],
    }


def write(name: str, value: Any) -> None:
    (HERE / name).write_text(json.dumps(value, indent=1) + "\n", encoding="ascii")


if __name__ == "__main__":
    write("models.json", models())
    write("schemas.json", schemas())
    write("functions.json", functions())
