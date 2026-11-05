"""Run an explicitly pinned upstream grader inside its own prepared environment, with every provider call metered.

The parent starts this script with a prepared virtualenv's Python, a clean environment and a JSON request on stdin.
Upstream scoring code is untouched: only its HTTP transport gains a request hook. The hook runs before every
network attempt (SDK retries included), refuses a request that would pass a pricing, request or token ceiling, and
appends a durable receipt before the request leaves and another when the response (or its absence) is known.
"""

import asyncio
import contextlib
import hashlib
import importlib
import importlib.metadata
import inspect
import io
import json
import math
import os
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

JUDGE_HOST = "api.openai.com"
BILLING_KEYS = {
    "input_usd_per_mtok",
    "output_usd_per_mtok",
    "max_requests",
    "max_input_tokens",
    "max_output_tokens",
    "image_tokens_per_image",
    "budget_usd",
}
CONTEXT_PRICING_KEYS = {"context_threshold_tokens", "context_input_usd_per_mtok", "context_output_usd_per_mtok"}


class BillingError(RuntimeError):
    """A provider call was refused before it was made, or its charge cannot be bounded."""


class SeamError(RuntimeError):
    """The pinned upstream no longer exposes the call seam that the meter relies on."""


def positive(value: Any) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise BillingError("billing limits must be positive finite numbers")
    return float(value)


def judge_charge(billing: dict[str, Any], input_tokens: int, output_tokens: int) -> float:
    """A long-context tier prices the whole request, including its output."""
    input_rate = positive(billing["input_usd_per_mtok"])
    output_rate = positive(billing["output_usd_per_mtok"])
    present = CONTEXT_PRICING_KEYS & billing.keys()
    if present:
        if present != CONTEXT_PRICING_KEYS:
            raise BillingError("context pricing needs its threshold and both rates")
        threshold = billing["context_threshold_tokens"]
        positive(threshold)
        if not isinstance(threshold, int):
            raise BillingError("context pricing threshold must be a whole token count")
        context_input = positive(billing["context_input_usd_per_mtok"])
        context_output = positive(billing["context_output_usd_per_mtok"])
        if context_input < input_rate or context_output < output_rate:
            raise BillingError("context pricing rates cannot reduce a conservative reservation")
        if input_tokens > threshold:
            input_rate, output_rate = context_input, context_output
    return (input_tokens * input_rate + output_tokens * output_rate) / 1_000_000


class Meter:
    """Pre-call ceilings plus a fsynced receipt log. Unsettled reservations stay charged at their worst case."""

    def __init__(self, billing: dict[str, Any], receipts: Path, host: str = JUDGE_HOST) -> None:
        if set(billing) not in (BILLING_KEYS, BILLING_KEYS | CONTEXT_PRICING_KEYS):
            raise BillingError("billing needs explicit pricing, request, token and dollar ceilings")
        judge_charge(billing, 0, 0)
        self.limits = {key: positive(value) for key, value in billing.items()}
        if "context_threshold_tokens" in billing:
            self.limits["context_threshold_tokens"] = billing["context_threshold_tokens"]
        self.receipts = receipts
        self.host = host
        self.lock = threading.Lock()
        self.requests = 0
        self.input_bound = 0
        self.settled_usd = 0.0
        self.open_usd: dict[int, float] = {}
        self.refusal: str | None = None
        # Receipts written by an earlier process for this same task count against the same ceilings.
        for row in self.read(receipts):
            if row["event"] == "reserved":
                self.requests += 1
                self.input_bound += row["input_bound"]
                self.open_usd[row["n"]] = row["reserved_usd"]
            elif row["event"] == "settled":
                self.open_usd.pop(row["n"], None)
                self.settled_usd += row["usd"]
        self.receipts.parent.mkdir(parents=True, exist_ok=True)

    @staticmethod
    def read(path: Path) -> list[dict[str, Any]]:
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

    def append(self, row: dict[str, Any]) -> None:
        descriptor = os.open(self.receipts, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(descriptor, (json.dumps(row) + "\n").encode())
            os.fsync(descriptor)
        finally:
            os.close(descriptor)

    @property
    def committed_usd(self) -> float:
        return self.settled_usd + sum(self.open_usd.values())

    def refuse(self, reason: str) -> None:
        self.refusal = self.refusal or reason
        self.append({"event": "refused", "reason": reason})
        raise BillingError(reason)

    def before(self, request: Any) -> None:
        with self.lock:
            if self.refusal is not None:
                raise BillingError(self.refusal)
            if (
                request.url.host != self.host
                or request.method != "POST"
                or not request.url.path.endswith("/chat/completions")
            ):
                self.refuse("request is not a chat completion to the pinned judge endpoint")
            try:
                body = request.content
                payload = json.loads(body)
            except Exception:
                self.refuse("request body is unreadable, so its size cannot be bounded")
            ceiling = payload.get("max_completion_tokens", payload.get("max_tokens"))
            if isinstance(ceiling, bool) or not isinstance(ceiling, int) or ceiling < 1:
                self.refuse("request does not declare an output token ceiling")
            if ceiling > self.limits["max_output_tokens"]:
                self.refuse("request output ceiling exceeds the approved maximum")
            images = 0

            def text_payload(value: Any) -> Any:
                nonlocal images
                if isinstance(value, list):
                    return [text_payload(item) for item in value]
                if isinstance(value, dict):
                    if value.get("type") == "image_url" and isinstance(value.get("image_url"), dict):
                        images += 1
                        # The provider decodes image URLs as images, so base64 bytes are not text tokens.
                        image = {**value["image_url"], "url": ""}
                        return {**value, "image_url": image}
                    return {key: text_payload(item) for key, item in value.items()}
                return value

            text_bytes = json.dumps(text_payload(payload), ensure_ascii=False).encode()
            input_bound = len(text_bytes) + self.limits["image_tokens_per_image"] * images
            reserved = judge_charge(self.limits, input_bound, ceiling)
            if self.requests + 1 > self.limits["max_requests"]:
                self.refuse("request ceiling reached")
            if self.input_bound + input_bound > self.limits["max_input_tokens"]:
                self.refuse("input token ceiling reached")
            if self.committed_usd + reserved > self.limits["budget_usd"]:
                self.refuse("dollar ceiling reached")
            self.requests += 1
            self.input_bound += input_bound
            self.open_usd[self.requests] = reserved
            request.extensions["billing_receipt"] = self.requests
            self.append(
                {
                    "event": "reserved",
                    "n": self.requests,
                    "input_bound": input_bound,
                    "max_output": ceiling,
                    "reserved_usd": reserved,
                }
            )

    def after(self, response: Any) -> None:
        number = response.request.extensions.get("billing_receipt")
        body: Any = None
        with contextlib.suppress(Exception):
            body = json.loads(response.content)
        body = body if isinstance(body, dict) else {}
        usage = body.get("usage")
        with self.lock:
            if number not in self.open_usd:
                return
            reserved = self.open_usd[number]
            tokens = (
                (
                    usage.get("prompt_tokens", usage.get("input_tokens")),
                    usage.get("completion_tokens", usage.get("output_tokens")),
                )
                if isinstance(usage, dict)
                else (None, None)
            )
            if all(isinstance(t, int) and not isinstance(t, bool) and t >= 0 for t in tokens):
                usd = judge_charge(self.limits, tokens[0], tokens[1])
            else:
                # No usable usage (including an error body): the reserved worst case stays charged.
                usd = reserved
            del self.open_usd[number]
            self.settled_usd += usd
            self.append(
                {
                    "event": "settled",
                    "n": number,
                    "status": response.status_code,
                    "input_tokens": tokens[0],
                    "output_tokens": tokens[1],
                    "usd": usd,
                    "overran_reservation": usd > reserved,
                    # What the endpoint says served the request: the receipt names the judge model it billed.
                    "model": body.get("model"),
                    "response_id": body.get("id"),
                }
            )

    def close(self, error: str | None) -> dict[str, Any]:
        with self.lock:
            summary = {
                "requests": self.requests,
                "committed_usd": self.committed_usd,
                "settled_usd": self.settled_usd,
                "unsettled_requests": len(self.open_usd),
                "refusal": self.refusal,
                "error_type": error,
            }
            self.append({"event": "closed", **summary})
            return summary

    async def abefore(self, request: Any) -> None:
        self.before(request)

    async def aafter(self, response: Any) -> None:
        await response.aread()
        self.after(response)

    def sync_after(self, response: Any) -> None:
        response.read()
        self.after(response)

    def async_client(self) -> Any:
        import httpx

        return httpx.AsyncClient(event_hooks={"request": [self.abefore], "response": [self.aafter]})

    def sync_client(self) -> Any:
        import httpx

        return httpx.Client(event_hooks={"request": [self.before], "response": [self.sync_after]})


def versions(*names: str) -> dict[str, str]:
    return {name: importlib.metadata.version(name) for name in names}


def require_marker(obj: Any, marker: str) -> None:
    if marker not in inspect.getsource(obj):
        raise SeamError(f"upstream no longer contains the metered call seam {marker!r}")


def probe_bu() -> dict[str, Any]:
    import dataclasses

    from browser_use.llm import ChatOpenAI

    if "http_client" not in {f.name for f in dataclasses.fields(ChatOpenAI)}:
        raise SeamError("ChatOpenAI has no http_client field")
    require_marker(ChatOpenAI.ainvoke, "self.get_client().chat.completions.create")
    require_marker(ChatOpenAI.get_client, "AsyncOpenAI(**client_params)")
    return {"seams_ok": True, "versions": versions("browser-use", "openai", "httpx")}


def probe_mind2web() -> dict[str, Any]:
    import backoff  # noqa: F401
    import PIL  # noqa: F401
    from utils import OpenaiEngine

    require_marker(OpenaiEngine.__init__, "self.client = OpenAI(")
    require_marker(OpenaiEngine.generate, "self.client.chat.completions.create")
    return {"seams_ok": True, "versions": versions("openai", "httpx", "backoff", "pillow")}


def judge_bu(request: dict[str, Any], meter: Meter) -> dict[str, Any]:
    import evaluation

    task = next(t for t in evaluation.load_tasks("BU_Bench_V2") if t["task_id"] == request["task_id"])
    judge = evaluation.create_judge("BU_Bench_V2", model=request["judge_model"], reasoning_effort=request["reasoning"])
    judge.http_client = meter.async_client()
    # Fail closed: the client every upstream call builds must carry the meter's transport.
    if judge.get_client()._client is not judge.http_client:
        raise SeamError("the judge client does not use the metered transport")
    output = asyncio.run(
        evaluation.judge_trace(task, request["trace"], judge, "BU_Bench_V2", artifact_dir=Path(request["artifact"]))
    )
    output["judge_config"] = evaluation.judge_config("BU_Bench_V2", judge)
    return output


def reasoning_generate(model: Any, request: dict[str, Any]) -> Any:
    """Keep upstream messages while declaring the reasoning judge's approved completion ceiling."""

    def generate(messages: Any) -> list[str]:
        response = model.client.chat.completions.create(
            model=request["judge_model"],
            messages=messages,
            max_completion_tokens=request["billing"]["max_output_tokens"],
            reasoning_effort=request["reasoning"],
        )
        # Upstream parses missing text as failure, so a truncated judge must stop before that parser.
        if not complete(response):
            raise ValueError("reasoning judge returned no complete verdict")
        return [choice.message.content for choice in response.choices]

    return generate


def complete(response: Any) -> bool:
    """Whether every choice finished with text."""
    return bool(response.choices) and all(
        choice.finish_reason == "stop" and choice.message.content for choice in response.choices
    )


def uncut_generate(model: Any, request: dict[str, Any]) -> Any:
    """Upstream's request, asked again at the approved completion ceiling when 512 tokens cut the answer off. A reply
    cut before its score or status reads to upstream's parser as a bad frame or a failed task, not as a judge failure."""

    def generate(messages: Any, max_new_tokens: int = 512, temperature: float = 0, **kwargs: Any) -> list[str]:
        for limit in (max_new_tokens, request["billing"]["max_output_tokens"]):
            response = model.client.chat.completions.create(
                model=model.model, messages=messages, max_tokens=limit, temperature=temperature, **kwargs
            )
            if complete(response):
                return [choice.message.content for choice in response.choices]
        raise ValueError("judge returned no complete answer at its approved ceiling")

    return generate


def labelled_as_sent(messages: Any) -> Any:
    """Name each attached image by its own bytes. Online-Mind2Web's final prompt encodes JPEG and declares PNG, which
    OpenAI ignores and Anthropic's API refuses with a 400, leaving every run that reaches a verdict ungraded."""
    wrong, right = "data:image/png;base64,/9j/", "data:image/jpeg;base64,/9j/"
    return json.loads(json.dumps(messages).replace(wrong, right))


def judge_mind2web(request: dict[str, Any], meter: Meter) -> dict[str, Any]:
    from methods.webjudge_online_mind2web import WebJudge_Online_Mind2Web_eval
    from utils import OpenaiEngine, extract_predication

    model = OpenaiEngine(model=request["judge_model"], api_key=os.environ["OPENAI_API_KEY"])
    transport = meter.sync_client()
    from openai import OpenAI

    model.client = OpenAI(api_key=os.environ["OPENAI_API_KEY"], http_client=transport)
    if model.client._client is not transport:
        raise SeamError("the judge client does not use the metered transport")
    generate = (reasoning_generate if request["reasoning_model"] else uncut_generate)(model, request)
    model.generate = lambda messages, *args, **kwargs: generate(labelled_as_sent(messages), *args, **kwargs)
    messages, text, system, record, points = asyncio.run(
        WebJudge_Online_Mind2Web_eval(
            request["task"], request["actions"], request["screenshots"], model, request["score_threshold"]
        )
    )
    response = model.generate(messages)[0]
    label = extract_predication(response, "WebJudge_Online_Mind2Web_eval")
    if label not in (0, 1):
        raise ValueError("upstream judge did not return a binary result")
    output = {"passed": bool(label), "score": float(label), "judge_model": request["judge_model"]}
    artifact = Path(request["artifact"])
    artifact.mkdir(parents=True, exist_ok=True)
    (artifact / "judge.json").write_text(
        json.dumps(
            {
                "messages": messages,
                "text": text,
                "system": system,
                "record": record,
                "points": points,
                "response": response,
                "result": output,
                "key_points_sha256": hashlib.sha256(str(points).encode()).hexdigest(),
            },
            default=str,
        )
    )
    return output


def regrade(request: dict[str, Any], bu: bool) -> dict[str, Any]:
    """Recalculate saved grades with upstream code without calling a model."""
    records = {}
    if bu:
        import evaluation

        tasks = {t["task_id"]: t for t in evaluation.load_tasks("BU_Bench_V2")}
    else:
        from utils import extract_predication
    # Keyed by run folder, one per task and repeat, each naming the task it graded.
    for key, record in request["records"].items():
        folder = Path(record["folder"])
        if bu:
            task = tasks[record["task"]]
            response = json.loads((folder / "judge" / "judge_response.json").read_text())
            if response.get("stop_reason") not in (None, "stop"):
                raise ValueError("saved judge did not finish")
            judgement = evaluation.findings_result_model(tuple(task["weights"])).model_validate(response["completion"])
            evaluation._validate_findings_coverage(judgement, tuple(task["weights"]))
            agent = json.loads((folder / "agent.json").read_text())
            actions = [json.dumps(json.loads(line)) for line in (folder / "steps.jsonl").read_text().splitlines()]
            files = {
                str(p.relative_to(folder / "downloads")): p.read_text()
                for p in (folder / "downloads").rglob("*")
                if p.is_file() and not _binary(p)
            }
            result = evaluation.score_findings(
                task, judgement, agent_texts=[agent.get("answer") or "No final result", *actions, json.dumps(files)]
            )
            records[key] = {"score": result["score"], "passed": result["verdict"]}
        else:
            saved = json.loads((folder / "judge" / "judge.json").read_text())
            label = extract_predication(saved["response"], "WebJudge_Online_Mind2Web_eval")
            if label not in (0, 1):
                raise ValueError("saved upstream grade is not binary")
            records[key] = {"score": float(label), "passed": bool(label)}
    return records


def _binary(path: Path) -> bool:
    try:
        path.read_text()
    except UnicodeDecodeError:
        return True
    return False


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=True).stdout.strip()


def main() -> None:
    os.umask(0o077)
    request = json.load(sys.stdin)
    root = Path(request["root"]).resolve()
    if sys.prefix == sys.base_prefix or root not in Path(sys.prefix).resolve().parents:
        raise ValueError("the judge Python must be the upstream checkout's prepared virtual environment")
    if git(root, "rev-parse", "HEAD") != request["commit"] or git(root, "status", "--porcelain"):
        raise ValueError("upstream checkout changed")
    suite, operation = request["suite"], request["operation"]
    bu = suite == "bu-bench"
    sys.path.insert(0, str(root if bu else root / "src"))
    meter: Meter | None = None
    error: str | None = None
    output: Any = None
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            if operation == "probe":
                output = probe_bu() if bu else probe_mind2web()
            elif operation == "regrade":
                output = regrade(request, bu)
            elif operation == "load":
                import evaluation

                output = evaluation.load_tasks("BU_Bench_V2")
            else:
                # The meter admits only the endpoint the run recorded; the SDK reads it from OPENAI_BASE_URL.
                base_url = request.get("base_url")
                host = urlparse(base_url).hostname if base_url else JUDGE_HOST
                meter = Meter(request["billing"], Path(request["receipts"]), host=host)
                output = (judge_bu if bu else judge_mind2web)(request, meter)
                output["billing"] = meter.close(None)
    except BaseException as exc:
        error = type(exc).__name__
        raise
    finally:
        if meter is not None and error is not None:
            meter.close(error)
    print(json.dumps(output))


if __name__ == "__main__":
    try:
        main()
    except BaseException as exc:
        # Type name only: upstream messages can quote prompts, task text or credentials.
        print(type(exc).__name__, file=sys.stderr)
        raise SystemExit(1) from None
