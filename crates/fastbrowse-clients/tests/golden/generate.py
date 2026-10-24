"""Record what the Python clients send and return, for the Rust clients to match.

    uv run python crates/fastbrowse-clients/tests/golden/generate.py

Each case builds a client, scripts the responses a fake transport (`httpx.MockTransport`) hands back in order, makes
one or more calls and records:

- every request: method, URL, the headers that matter (the bearer token blanked), the exact body bytes and the
  canonical body (sorted keys), which is what a replay is keyed on;
- what each call returned: the data and cost, or the error's type and message;
- the ledger's lines and call counts, and the lines a `jev_spend` scope collected;
- the trace records that say how a call went: spans, retries with their waits, failovers, cancelled request costs.

Retry backoff is recorded rather than slept: `asyncio.sleep` is replaced, as `tests/clients/conftest.py` does. Races
run on the real clock, so the cases with a stalled request take about the race delay each. Wall times are left out
(`seconds` is null, and "in 1.5s" in a message reads "in <t>s"): Python's clock and a paused tokio clock disagree.
"""

import asyncio
import base64
import json
import os
import re
from pathlib import Path
from typing import Any, Literal

import httpx

from fastbrowse.clients.environment import Settings
from fastbrowse.clients.failover import FailoverJevClient
from fastbrowse.clients.openai_compatible import (
    GATEWAY_PROVIDER_ROUTING,
    OPENROUTER_PROVIDER_ROUTING,
    OpenAICompatibleLLM,
    ReasoningEffort,
)
from fastbrowse.clients.typesafe import TypeSafeJevClient
from fastbrowse.clients.validation import jev_spend
from fastbrowse.clients.vercel import VercelGatewayJevClient
from fastbrowse.jev import ChoiceQuestion, JevRetriesExhausted, NoulQuestion, Question, ScoreQuestion
from fastbrowse.llm import Message
from fastbrowse.models import CostLine, Frozen, Limits, LLMPurpose
from fastbrowse.telemetry import Ledger, traced

OUT = Path(__file__).with_name("cases.json")


class Result(Frozen):
    count: int
    tags: tuple[str, ...] = ()
    """What was counted."""


class Answer(Frozen):
    answer: str


class Requirement(Frozen):
    text: str
    kind: Literal["fact", "action"]


class Plan(Frozen):
    """What the run must show before it may finish."""

    requirements: tuple[Requirement, ...]
    answer_expected: bool
    note: str | None = None


SCHEMAS = {"Result": Result, "Answer": Answer, "Plan": Plan}
PNG = b"\x89PNG\r\n\x1a\nimage-bytes"
JPEG = b"\xff\xd8\xffjpeg-bytes"
KEPT_HEADERS = (
    "authorization",
    "content-type",
    "ai-gateway-auth-method",
    "ai-gateway-protocol-version",
    "ai-model-id",
    "ai-evaluation-model-specification-version",
)
SETTINGS_VARIABLES = re.compile(
    r"^(FASTBROWSE_|TYPESAFE_API_KEY$|AI_GATEWAY_API_KEY$|OPENROUTER_API_KEY$|BROWSER_USE_API_KEY$)", re.IGNORECASE
)


def ok(body: Any, status: int = 200, headers: dict[str, str] | None = None) -> dict[str, Any]:
    text = body if isinstance(body, str) else json.dumps(body)
    return {"status": status, "headers": headers or {}, "body": text}


def completion(content: str | None, *, cost: float | None = 0.001, finish: str | None = None, **extra: Any) -> dict:
    usage: dict[str, Any] = {"prompt_tokens": 20, "completion_tokens": 5}
    if cost is not None:
        usage["cost"] = cost
    choice: dict[str, Any] = {"message": {"content": content}}
    if finish is not None:
        choice["finish_reason"] = finish
    return ok({"choices": [choice], "usage": usage, **extra})


def noul(key: str, value: float = 1, *, gateway: bool = False) -> dict:
    return {key: {"type": "boolean", "probability": value} if gateway else {"type": "noul", "noul": value}}


def jev_answer(answers: dict, *, tokens: int = 100, cost: Any = None, gateway: bool = False, confidence=None) -> dict:
    if gateway:
        metadata: dict[str, Any] = {"typesafe": {"confidence": confidence or {}}}
        if cost is not None:
            metadata["gateway"] = {"cost": cost}
        return ok(
            {"answers": answers, "usage": {"inputTokens": tokens, "outputTokens": 7}, "providerMetadata": metadata}
        )
    usage: dict[str, Any] = {"input_tokens": tokens, "output_tokens": 7}
    if cost is not None:
        usage["cost"] = cost
    return ok({"answers": answers, "usage": usage})


class Script:
    """Hands back scripted responses in the order requests arrive, and records each request."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = list(responses)
        self.requests: list[dict[str, Any]] = []
        self.cancel = asyncio.Event()

    async def handler(self, request: httpx.Request) -> httpx.Response:
        body = request.content.decode()
        headers = {name: request.headers[name] for name in KEPT_HEADERS if name in request.headers}
        if "authorization" in headers:
            headers["authorization"] = "Bearer ***"
        canonical = (
            json.dumps(json.loads(body), sort_keys=True, separators=(",", ":"), ensure_ascii=False) if body else ""
        )
        self.requests.append(
            {
                "method": request.method,
                "url": str(request.url),
                "headers": headers,
                "body": body,
                "canonical": canonical,
            }
        )
        if not self.responses:
            raise AssertionError(f"no scripted response for request {len(self.requests)}: {request.url}")
        step = self.responses.pop(0)
        if step.get("stall"):
            await asyncio.Event().wait()
        if step.get("cancel"):
            self.cancel.set()
            await asyncio.Event().wait()
        if "raise" in step:
            raise getattr(httpx, step["raise"])("scripted failure", request=request)
        return httpx.Response(step["status"], headers=step["headers"], content=step["body"].encode())


def env_settings(env: dict[str, str]) -> Settings:
    saved = dict(os.environ)
    try:
        for name in list(os.environ):
            if SETTINGS_VARIABLES.match(name):
                del os.environ[name]
        os.environ.update(env)
        return Settings(_env_file=None)
    finally:
        os.environ.clear()
        os.environ.update(saved)


def build(spec: dict[str, Any], http: httpx.AsyncClient) -> Any:
    kind = spec["kind"]
    if kind == "llm":
        routing = spec.get("provider_routing", "openrouter")
        routing = {"openrouter": OPENROUTER_PROVIDER_ROUTING, "gateway": GATEWAY_PROVIDER_ROUTING}.get(routing, routing)
        effort = spec.get("reasoning")
        return OpenAICompatibleLLM(
            spec.get("api_key", "key"),
            http=http,
            base_url=spec.get("base_url", "https://openrouter.ai/api/v1"),
            models={LLMPurpose(purpose): model for purpose, model in spec["models"].items()},
            reasoning_effort=None if effort is None else ReasoningEffort(effort),
            provider_routing=routing,
            strict=spec.get("strict", True),
            extra_body=spec.get("extra_body"),
        )
    if kind == "typesafe":
        return TypeSafeJevClient(spec.get("api_key", "key"), http=http, base_url=spec["base_url"], model=spec["model"])
    if kind == "gateway":
        return VercelGatewayJevClient(spec.get("api_key", "key"), http=http, base_url=spec["base_url"])
    if kind == "failover":
        return FailoverJevClient(build(spec["primary"], http), build(spec["backup"], http))
    if kind == "settings":
        settings = env_settings(spec["env"])
        return settings.jev(http) if spec["which"] == "jev" else settings.llm(http)
    raise ValueError(kind)


def normalized(text: str) -> str:
    return re.sub(r"in \d+\.\ds", "in <t>s", text)


def cost_dump(cost: CostLine) -> dict[str, Any]:
    return cost.model_dump(mode="json") | {"seconds": None}


def outcome(result: Any) -> dict[str, Any]:
    if isinstance(result, BaseException):
        error: dict[str, Any] = {"type": type(result).__name__, "message": normalized(str(result))}
        if isinstance(result, JevRetriesExhausted):
            error |= {
                "requests": result.requests,
                "unaccounted_requests": result.unaccounted_requests,
                "answered": [key for part in result.answered for key in part.answers],
                "last": result.last,
            }
        return {"error": error}
    if hasattr(result, "data"):
        return {"ok": {"data": result.data.model_dump(mode="json"), "cost": cost_dump(result.cost)}}
    dumped = result.model_dump(mode="json")
    dumped["cost"]["seconds"] = None
    return {"ok": dumped}


def traced_records(events: list[Any]) -> list[dict[str, Any]]:
    kept = []
    for event in events:
        if not isinstance(event, dict):
            continue
        match event["event"]:
            case "call":
                kept.append({"event": "call", "call": event["call"], "outcome": event["outcome"]})
            case "request_retry":
                kept.append({key: event[key] for key in ("event", "call", "attempt", "reason", "wait")})
            case "jev_failover":
                kept.append({"event": "jev_failover", "reason": normalized(event["reason"])})
            case "cancelled_request_cost":
                kept.append(dict(event))
    return kept


async def run_case(case: dict[str, Any]) -> dict[str, Any]:
    script = Script(case["responses"])
    ledger = Ledger(Limits(**case.get("limits", {})))
    results, ledgers, spends = [], [], []
    with traced() as events:
        async with httpx.AsyncClient(transport=httpx.MockTransport(script.handler)) as http:
            client = build(case["client"], http)
            for call in case["calls"]:
                paid: list[CostLine] = []
                if call["op"] == "generate":
                    messages = [
                        Message(
                            role=m["role"],
                            content=m["content"],
                            images=tuple(base64.b64decode(i) for i in m.get("images", ())),
                        )
                        for m in call["messages"]
                    ]
                    coroutine = client.generate(
                        LLMPurpose(call["purpose"]),
                        messages,
                        SCHEMAS[call["schema"]],
                        max_output_tokens=call.get("max_output_tokens", 2000),
                        ledger=ledger if call.get("ledger", True) else None,
                    )
                    scope = None
                else:
                    questions: dict[str, Question] = {key: _question(value) for key, value in call["questions"].items()}
                    coroutine = client.evaluate(call["state"], questions)
                    scope = call.get("spend")
                if scope == "ledger":
                    with jev_spend(ledger.lines, ledger=ledger):
                        task = asyncio.create_task(coroutine)
                elif scope == "paid":
                    with jev_spend(paid):
                        task = asyncio.create_task(coroutine)
                else:
                    task = asyncio.create_task(coroutine)
                if call.get("cancel"):
                    await script.cancel.wait()
                    script.cancel.clear()
                    task.cancel()
                try:
                    result: Any = await task
                except BaseException as error:  # every outcome is recorded, cancellation included
                    result = error
                results.append(outcome(result))
                ledgers.append(
                    {
                        "lines": [cost_dump(line) for line in ledger.lines],
                        "llm_calls": ledger.llm_calls,
                        "jev_calls": ledger.jev_calls,
                    }
                )
                spends.append([cost_dump(line) for line in paid])
    assert not script.responses, f"{case['name']}: {len(script.responses)} scripted responses unused"
    return case | {
        "requests": script.requests,
        "results": results,
        "ledgers": ledgers,
        "spends": spends,
        "trace": traced_records(events),
    }


def _question(value: dict[str, Any]) -> Question:
    match value["type"]:
        case "choice":
            return ChoiceQuestion.model_validate(value)
        case "noul":
            return NoulQuestion.model_validate(value)
        case _:
            return ScoreQuestion.model_validate(value)


def q_choice(**criteria: Any) -> dict[str, Any]:
    return {"type": "choice", "instructions": "Which colour?", "criteria": criteria or {"blue": "Blue", "red": "Red"}}


def q_noul(instructions: str = "Is it?", true: str | None = None, false: str | None = None) -> dict[str, Any]:
    return {"type": "noul", "instructions": instructions, "true": true, "false": false}


def q_score() -> dict[str, Any]:
    return {"type": "score", "instructions": "How much?", "criteria": ["none", "some", "all"]}


ALL_MODELS = {purpose.value: f"model-{purpose.value}" for purpose in LLMPurpose}
OPENROUTER_LLM = {"kind": "llm", "models": ALL_MODELS, "reasoning": "low"}
# A host without receipts or price lists, for the cases whose charge would otherwise be looked up.
PLAIN_LLM = {**OPENROUTER_LLM, "base_url": "https://llm.test/v1"}
USER = [{"role": "system", "content": "You count things."}, {"role": "user", "content": "Count the apples."}]


def generate(purpose: str = "read", schema: str = "Result", **extra: Any) -> dict[str, Any]:
    return {"op": "generate", "purpose": purpose, "messages": USER, "schema": schema, **extra}


def evaluate(questions: dict[str, Any], state: Any = "page", **extra: Any) -> dict[str, Any]:
    return {"op": "evaluate", "state": state, "questions": questions, **extra}


DIRECT = {"kind": "typesafe", "base_url": "https://api.typesafe.ai", "model": "jev-1.13.0"}
OPENROUTER_JEV = {"kind": "typesafe", "base_url": "https://openrouter.ai/api", "model": "jev-1.13"}
GATEWAY = {"kind": "gateway", "base_url": "https://ai-gateway.vercel.sh"}
GATEWAY_ANSWERS = {
    "green": {"type": "boolean", "probability": 0.02},
    "col": {"type": "choice", "choice": "blue", "probabilities": {"blue": 1, "red": 0}},
    "sc": {"type": "score", "score": 1.99, "probabilities": {"0": 0, "1": 0, "2": 1}},
}
DIRECT_ANSWERS = {
    "col": {"type": "choice", "choice": "blue", "probabilities": {"blue": 0.9, "red": 0.1}, "confidence": 0.8},
    "bare": {"type": "noul", "noul": 0},
    "green": {"type": "noul", "noul": 0.02},
    "partial": {"type": "noul", "noul": 1},
    "score": {"type": "score", "score": 1.99, "probabilities": {"0": 0, "1": 0, "2": 1}, "confidence": 0.99},
}
DIRECT_QUESTIONS = {
    "col": q_choice(),
    "bare": q_noul("Wrong?"),
    "green": q_noul("Green?", "green", "not green"),
    "partial": q_noul("Present?", "present"),
    "score": q_score(),
}
FAIL_503 = ok({"error": {"message": "Service temporarily unavailable"}}, 503)


def cases() -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []

    def case(name: str, client: dict, calls: list, responses: list, **extra: Any) -> None:
        found.append({"name": name, "client": client, "calls": calls, "responses": responses, **extra})

    # Each purpose, on OpenRouter's defaults: its model, its cap, reasoning and routing.
    for purpose in LLMPurpose:
        case(
            f"llm {purpose.value}",
            OPENROUTER_LLM,
            [generate(purpose.value, max_output_tokens=321)],
            [completion('{"count":3,"tags":["a","b"]}')],
        )
    case(
        "llm images",
        OPENROUTER_LLM,
        [
            generate(
                messages=[
                    {
                        "role": "user",
                        "content": "Count",
                        "images": [base64.b64encode(PNG).decode(), base64.b64encode(JPEG).decode()],
                    }
                ]
            )
        ],
        [completion('{"count":2}')],
    )
    case(
        "llm nested schema",
        OPENROUTER_LLM,
        [generate("plan", "Plan")],
        [completion('{"requirements":[{"text":"Report the price","kind":"fact"}],"answer_expected":true,"note":null}')],
    )
    case(
        "llm unicode prompt",
        OPENROUTER_LLM,
        [generate(messages=[{"role": "user", "content": 'Café — \U0001f600 "quoted" \\ \n\t\u0001'}])],
        [completion('{"count":1,"tags":["été"]}')],
    )
    case(
        "llm gateway route",
        {**OPENROUTER_LLM, "base_url": "https://ai-gateway.vercel.sh/v1", "provider_routing": "gateway"},
        [generate()],
        [completion('{"count":1}')],
    )
    case(
        "llm local non-strict extra body",
        {
            "kind": "llm",
            "api_key": "",
            "base_url": "http://127.0.0.1:8000/v1/",
            "models": {"read": "qwen"},
            "provider_routing": None,
            "strict": False,
            "extra_body": {
                "chat_template_kwargs": {"enable_thinking": False},
                "top_k": 20,
                "temperature": 0.7,
                "min_p": 1e-05,
                "big": 1e16,
                "seed": 12345678901234,
            },
        },
        [generate()],
        [completion('{"count":1}', cost=None)],
    )
    case(
        "llm local strict with reasoning",
        {
            "kind": "llm",
            "api_key": "local",
            "base_url": "http://127.0.0.1:8000/v1",
            "models": {"read": "qwen"},
            "provider_routing": None,
            "reasoning": "high",
        },
        [generate()],
        [completion('{"count":1}', cost=0.0)],
    )
    case(
        "llm schema repair",
        OPENROUTER_LLM,
        [generate(max_output_tokens=42)],
        [completion('{"count":"wrong"}'), completion('{"count":3}')],
    )
    case(
        "llm repair fails twice",
        OPENROUTER_LLM,
        [generate("plan")],
        [completion('{"count":"wrong"}'), completion('{"tags":[1]}')],
    )
    case(
        "llm invalid json repaired",
        OPENROUTER_LLM,
        [generate()],
        [
            completion("not json"),
            completion('{"count":1} x'),
        ],
    )
    case(
        "llm extra field repaired",
        OPENROUTER_LLM,
        [generate()],
        [completion('{"count": 1, "local": 2}'), completion('{"count":1}')],
    )
    case(
        "llm truncated then larger cap",
        OPENROUTER_LLM,
        [generate(max_output_tokens=100)],
        [completion('{"count":"cut off mid-str', finish="length"), completion('{"count":5}', finish="stop")],
    )
    case(
        "llm truncated twice",
        OPENROUTER_LLM,
        [generate(max_output_tokens=100)],
        [completion('{"count":"', finish="length"), completion('{"count":"', finish="length")],
    )
    case(
        "llm ends mid json short of cap",
        OPENROUTER_LLM,
        [generate(max_output_tokens=100)],
        [completion('{"count":"cut off'), completion('{"count":"cut off')],
    )
    case(
        "llm short reply after repair",
        OPENROUTER_LLM,
        [generate(max_output_tokens=100)],
        [completion('{"count":"five"}'), completion('{"count":"cut off'), completion('{"count":5}')],
    )
    case(
        "llm empty choices then answer",
        PLAIN_LLM,
        [generate()],
        [ok({"choices": [], "usage": {"cost": 0.02, "prompt_tokens": "invalid"}}), completion('{"count":1}')],
    )
    case("llm refusal twice", OPENROUTER_LLM, [generate()], [completion(None), completion(None)])
    case(
        "llm 503 then answer",
        OPENROUTER_LLM,
        [generate()],
        [FAIL_503, completion('{"count":1}')],
        limits={"max_llm_calls": 3},
    )
    case(
        "llm 429 retry-after then answer",
        OPENROUTER_LLM,
        [generate()],
        [ok("rate limited", 429, {"retry-after-ms": "250"}), completion('{"count":1}')],
    )
    case("llm retries exhausted", OPENROUTER_LLM, [generate()], [ok("rate limited" + "x" * 600, 429)] * 6)
    case("llm timeout then answer", OPENROUTER_LLM, [generate()], [{"raise": "ReadTimeout"}, completion('{"count":1}')])
    case(
        "llm 401",
        {**OPENROUTER_LLM, "api_key": "sk-secret"},
        [generate()],
        [ok({"error": {"type": "auth", "message": "bad key sk-secret"}}, 401)],
    )
    case(
        "llm receipt lookup",
        OPENROUTER_LLM,
        [generate("field_text")],
        [
            completion('{"count":3}', cost=None, id="generation-1"),
            ok({"data": {"id": "generation-1", "total_cost": 0.001}}),
        ],
    )
    case(
        "llm byok receipt",
        {**OPENROUTER_LLM, "base_url": "https://ai-gateway.vercel.sh/v1"},
        [generate("field_text")],
        [
            completion('{"count":3}', cost=None, id="generation-byok"),
            ok(
                {
                    "data": {
                        "id": "generation-byok",
                        "is_byok": True,
                        "total_cost": 0.001,
                        "upstream_inference_cost": 0.3,
                    }
                }
            ),
        ],
    )
    case(
        "llm receipt retried then bounded",
        OPENROUTER_LLM,
        [generate("plan")],
        [
            completion('{"count":3}', cost=None, id="generation-1"),
            ok({"data": {"id": "generation-1", "total_cost": None}}),
            ok("not found", 404),
            ok({"data": {"id": "generation-1", "total_cost": -1}}),
            ok(
                {
                    "data": {
                        "id": "writer",
                        "endpoints": [
                            {
                                "context_length": 1000,
                                "pricing": {"prompt": "0.000001", "completion": "0.000004", "request": "0"},
                            },
                            {
                                "context_length": 2000,
                                "pricing": {
                                    "prompt": "0.000002",
                                    "completion": "0.000003",
                                    "image": "0.01",
                                    "discount": 0.5,
                                },
                            },
                        ],
                    }
                }
            ),
        ],
        limits={"max_dollars": 28.0},
    )
    case(
        "llm transport failed then bounded",
        OPENROUTER_LLM,
        [
            generate(
                "field_text",
                messages=[{"role": "user", "content": "Write", "images": [base64.b64encode(PNG).decode()]}],
            )
        ],
        [{"raise": "ReadTimeout"}] * 6
        + [
            ok(
                {
                    "data": {
                        "endpoints": [
                            {
                                "context_length": 2000,
                                "pricing": {"prompt": "0.000002", "completion": "0.000004", "image": "0.01"},
                            }
                        ]
                    }
                }
            )
        ],
        limits={"max_dollars": 28.0},
    )
    case(
        "llm race won by the second request",
        OPENROUTER_LLM,
        [generate("field_text")],
        [{"stall": True}, completion('{"count":1}', cost=0.01)],
        limits={"max_llm_calls": 2, "max_dollars": 1.0},
    )
    case(
        "llm cancelled mid request",
        PLAIN_LLM,
        [generate("plan", cancel=True)],
        [{"cancel": True}],
        limits={"max_dollars": 1.0},
        attempt_seconds=0.05,
    )
    case(
        "llm cancelled after a paid repair",
        PLAIN_LLM,
        [generate("plan", cancel=True)],
        [completion('{"count":"wrong"}', cost=0.01), {"cancel": True}],
        limits={"max_dollars": 1.0},
        attempt_seconds=0.05,
    )
    case(
        "llm call limit stops a repair",
        OPENROUTER_LLM,
        [generate()],
        [completion('{"count":"wrong"}', cost=0.01)],
        limits={"max_llm_calls": 1},
    )
    case(
        "llm keyless local",
        {
            "kind": "llm",
            "api_key": "",
            "base_url": "http://127.0.0.1:8000/v1",
            "models": {"read": "qwen"},
            "provider_routing": None,
        },
        [generate()],
        [completion('{"count": 1, "local": 2}'), completion('{"count": 1, "local": 2}')],
    )
    case("llm unsupported scheme", PLAIN_LLM, [generate()], [{"raise": "UnsupportedProtocol"}])

    # Jev: every question kind on each wire, costs, refusals and splits.
    case(
        "jev direct all kinds",
        {**DIRECT, "base_url": "https://direct.test/", "model": "custom-jev"},
        [evaluate(DIRECT_QUESTIONS, {"text": "hello"})],
        [jev_answer(DIRECT_ANSWERS, tokens=389)],
    )
    case(
        "jev openrouter metered",
        OPENROUTER_JEV,
        [evaluate({"q": q_choice()}, "blue")],
        [
            ok(
                {
                    "model": "typesafe/jev-1.13-20260917",
                    "answers": {"q": DIRECT_ANSWERS["col"]},
                    "usage": {"input_tokens": 476, "output_tokens": 70, "cost": 0.00003},
                }
            )
        ],
    )
    case(
        "jev gateway all kinds",
        GATEWAY,
        [evaluate({"green": q_noul("Green?"), "col": q_choice(), "sc": q_score()}, "state")],
        [jev_answer(GATEWAY_ANSWERS, tokens=389, cost="0.000016338", gateway=True, confidence={"col": 1, "sc": 0.99})],
    )
    case(
        "jev gateway zero cost",
        GATEWAY,
        [evaluate({"green": q_noul("Green?")})],
        [jev_answer({"green": {"type": "boolean", "probability": 0.5}}, tokens=10, cost=0, gateway=True)],
    )
    case(
        "jev one option answered locally",
        DIRECT,
        [
            evaluate({"col": q_choice(), "fill_target": q_choice(e7="Search")}),
            evaluate({"fill_target": q_choice(e7="Search")}),
        ],
        [jev_answer({"col": DIRECT_ANSWERS["col"]}, tokens=9)],
    )
    case(
        "jev state encoding",
        DIRECT,
        [
            evaluate(
                {"q": q_noul()},
                {"price": 1e-05, "n": 1.0, "big": 1e16, "text": "café \U0001f600 </>", "list": [1, 2.5, None, True]},
            )
        ],
        [jev_answer(noul("q"))],
    )
    case("jev 503 then answer", DIRECT, [evaluate({"q": q_noul()})], [FAIL_503, jev_answer(noul("q"))])
    case(
        "jev input too large",
        GATEWAY,
        [evaluate({"q": q_noul()})],
        [
            ok(
                {
                    "error": {
                        "message": "typesafe returned status 400",
                        "type": "AI_APICallError",
                        "param": {"message": "max_tokens_exceeded", "statusCode": 400, "isRetryable": False},
                    },
                    "providerMetadata": {"gateway": {"routing": {"resolvedProvider": "typesafe-ai"}}},
                },
                400,
            )
        ],
    )
    case(
        "jev gateway retryable flag",
        GATEWAY,
        [evaluate({"q": q_noul()})],
        [
            ok(
                {
                    "error": {
                        "message": "typesafe returned status 409",
                        "type": "AI_APICallError",
                        "param": {"message": "the request conflicts", "isRetryable": True},
                    }
                },
                409,
            ),
            jev_answer(noul("q", gateway=True), gateway=True),
        ],
    )
    case(
        "jev invalid answer",
        DIRECT,
        [evaluate({"q": q_choice()})],
        [jev_answer({"q": {**DIRECT_ANSWERS["col"], "probabilities": {"blue": 0.3, "red": 0.3}}})],
    )
    case(
        "jev answer not maximal",
        DIRECT,
        [evaluate({"q": q_choice()})],
        [jev_answer({"q": {**DIRECT_ANSWERS["col"], "choice": "red"}})],
    )
    case("jev not json", GATEWAY, [evaluate({"q": q_noul()})], [ok("not json")])
    case("jev body a string", DIRECT, [evaluate({"q": q_noul()})], [ok('"sk-echoed"')])
    case(
        "jev invalid cost",
        DIRECT,
        [evaluate({"q": q_choice()})],
        [ok({"answers": {"q": DIRECT_ANSWERS["col"]}, "usage": {"input_tokens": 100, "cost": "not-a-cost"}})],
    )
    case("jev retries exhausted", DIRECT, [evaluate({"q": q_noul()})], [FAIL_503] * 6)
    case("jev transport failed", DIRECT, [evaluate({"q": q_noul()})], [{"raise": "ConnectError"}] * 6)
    case("jev unsupported scheme", DIRECT, [evaluate({"q": q_noul()})], [{"raise": "UnsupportedProtocol"}])
    case(
        "jev 401 echoes the key",
        {**GATEWAY, "api_key": "sk-a1b2c3d4a1b2c3d4"},
        [evaluate({"q": q_noul()})],
        [ok({"error": {"type": "auth", "message": "x" * 290 + " bad key sk-a1b2c3d4a1b2c3d4"}}, 401)],
    )
    case(
        "jev batch split",
        DIRECT,
        [evaluate({"a": q_noul("a"), "b": q_noul("b")}, spend="paid")],
        [FAIL_503, FAIL_503, jev_answer(noul("a")), jev_answer(noul("b"))],
    )
    case(
        "jev batch split exhausted",
        GATEWAY,
        [evaluate({"a": q_noul("a"), "b": q_noul("b")}, spend="paid")],
        [FAIL_503, FAIL_503, jev_answer(noul("a", gateway=True), gateway=True)] + [FAIL_503] * 4,
    )
    case(
        "jev race won by the second request",
        DIRECT,
        [evaluate({"q": q_noul()})],
        [{"stall": True}, jev_answer(noul("q"), cost=0.002)],
    )
    case(
        "jev cancelled mid request", DIRECT, [evaluate({"q": q_noul()}, spend="paid", cancel=True)], [{"cancel": True}]
    )
    case(
        "jev split single over the dollar cap",
        DIRECT,
        [evaluate({"a": q_noul("a" * 3000), "b": q_noul("b" * 3000)}, spend="ledger")],
        [FAIL_503, FAIL_503, jev_answer(noul("a"), tokens=int(0.001 / (0.042 / 1_000_000)) - 100)],
        limits={"max_dollars": 0.001},
    )

    # Failover between providers.
    primary = {**DIRECT, "base_url": "https://primary.test"}
    backup = {**GATEWAY, "base_url": "https://backup.test"}
    case(
        "failover after exhaustion",
        {"kind": "failover", "primary": primary, "backup": backup},
        [evaluate({"q": q_noul()}), evaluate({"q": q_noul()})],
        [FAIL_503] * 6 + [jev_answer(noul("q", gateway=True), gateway=True, cost="0.001")] * 2,
    )
    case(
        "failover both exhausted",
        {"kind": "failover", "primary": primary, "backup": backup},
        [evaluate({"q": q_noul()})],
        [{"raise": "ReadTimeout"}] + [FAIL_503] * 5 + [ok("busy", 429)] * 6,
    )
    case(
        "failover keeps answered singles",
        {"kind": "failover", "primary": primary, "backup": {**DIRECT, "base_url": "https://backup.test"}},
        [evaluate({"a": q_noul("a"), "b": q_noul("b")}, spend="paid")],
        [FAIL_503, FAIL_503, jev_answer(noul("a")), FAIL_503, FAIL_503, FAIL_503, jev_answer(noul("b"))],
    )

    # Settings from the environment, the way a run and the replay harness build their clients.
    keys = {"OPENROUTER_API_KEY": "o", "AI_GATEWAY_API_KEY": "g"}
    case(
        "settings openrouter jev",
        {"kind": "settings", "which": "jev", "env": keys},
        [evaluate({"q": q_noul()})],
        [jev_answer(noul("q"))],
    )
    case(
        "settings openrouter llm",
        {"kind": "settings", "which": "llm", "env": keys},
        [generate("plan", "Answer")],
        [completion('{"answer":"ok"}')],
    )
    case(
        "settings gateway llm",
        {"kind": "settings", "which": "llm", "env": {"AI_GATEWAY_API_KEY": "g", "FASTBROWSE_LLM_REASONING": "medium"}},
        [generate("field_text", "Answer")],
        [completion('{"answer":"ok"}')],
    )
    replay = {
        "OPENROUTER_API_KEY": "o",
        "FASTBROWSE_JEV_BASE_URL": "http://replay.test/jev/",
        "FASTBROWSE_LLM_BASE_URL": "http://replay.test/llm/v1",
        "FASTBROWSE_LLM_MODEL": "recorded-model",
    }
    case(
        "settings replay jev",
        {"kind": "settings", "which": "jev", "env": replay},
        [evaluate({"q": q_noul()})],
        [jev_answer(noul("q"))],
    )
    case(
        "settings replay llm",
        {"kind": "settings", "which": "llm", "env": replay},
        [generate("verify", "Answer")],
        [completion('{"answer":"ok"}')],
    )
    case(
        "settings typesafe with gateway backup",
        {"kind": "settings", "which": "jev", "env": {"TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g"}},
        [evaluate({"q": q_noul()})],
        [FAIL_503] * 6 + [jev_answer(noul("q", gateway=True), gateway=True)],
    )
    case(
        "settings local llm shaped",
        {
            "kind": "settings",
            "which": "llm",
            "env": {
                "FASTBROWSE_LLM_BASE_URL": "http://127.0.0.1:8000/v1/",
                "FASTBROWSE_LLM_MODEL": "qwen",
                "FASTBROWSE_LLM_MODEL_READ": "qwen-reader",
                "FASTBROWSE_LLM_API_KEY": " local-key ",
                "FASTBROWSE_LLM_STRICT": "0",
                "FASTBROWSE_LLM_EXTRA_BODY": '{"top_k": 20, "chat_template_kwargs": {"enable_thinking": false}}',
                "FASTBROWSE_LLM_REASONING": "high",
                "OPENROUTER_API_KEY": "o",
            },
        },
        [generate("read", "Answer")],
        [completion('{"answer":"ok"}', cost=None)],
    )
    return found


async def main() -> None:
    import fastbrowse.clients.openai_compatible as openai_compatible

    real_sleep = asyncio.sleep
    waits: list[float] = []

    async def recorded(delay: float) -> None:
        waits.append(delay)
        await real_sleep(0)

    asyncio.sleep = recorded  # ty: ignore[invalid-assignment]
    recorded_cases = []
    try:
        for case in cases():
            attempt = openai_compatible.LLM_ATTEMPT_SECONDS
            # Python lets a cancelled LLM request run to its attempt deadline before it settles; a short one keeps
            # the case quick without changing what is recorded.
            openai_compatible.LLM_ATTEMPT_SECONDS = case.get("attempt_seconds", attempt)
            waits.clear()
            try:
                recorded_case = await run_case(case)
            finally:
                openai_compatible.LLM_ATTEMPT_SECONDS = attempt
            recorded_case["sleeps"] = list(waits)
            recorded_cases.append(recorded_case)
            print(case["name"], [result.get("error", {}).get("type", "ok") for result in recorded_case["results"]])
    finally:
        asyncio.sleep = real_sleep
    OUT.write_text(json.dumps(recorded_cases, indent=1, ensure_ascii=True) + "\n")


if __name__ == "__main__":
    asyncio.run(main())
