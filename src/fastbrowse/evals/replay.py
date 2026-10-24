"""Record and replay the model traffic of a run at the HTTP layer, so any implementation of `fastbrowse serve` can
be driven by the same model responses.

The agent is pointed at a local proxy through FASTBROWSE_JEV_BASE_URL and FASTBROWSE_LLM_BASE_URL. Jev requests
arrive under `/jev` and LLM requests under `/llm`:

- `record` forwards each POST to OpenRouter with the real key, keeps the response and returns it.
- `replay` answers from what was kept, and answers a request it has never seen with HTTP 404.

A response is filed under the sha256 of the request path and the request body as canonical JSON (sorted keys,
values that vary between identical runs replaced by placeholders). Headers are never part of the key, so the
bearer token is not either. One key can hold more than one response: the agent races a slow call with an identical
twin, and retries a failed one with the same body. They are served in the order they were recorded, and the last
one again once they run out.

A miss is the signal that two runs asked different questions. Each is logged with its purpose and model and a
unified diff against the nearest body recorded on the same path.
"""

import difflib
import gzip
import hashlib
import json
import re
import sys
import threading
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from enum import StrEnum
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Self

import httpx

from fastbrowse.clients.environment import LLM_OPENROUTER_URL
from fastbrowse.clients.openai_compatible import OPENROUTER_PROVIDER_ROUTING
from fastbrowse.clients.typesafe import OPENROUTER_URL

CASSETTE_VERSION = 1

JEV_MOUNT = "/jev"
LLM_MOUNT = "/llm"

UPSTREAMS = {JEV_MOUNT: OPENROUTER_URL, LLM_MOUNT: LLM_OPENROUTER_URL}
"""Where `record` sends each mount: the hosts a run with only OPENROUTER_API_KEY set would call."""

MISS_STATUS = 404
"""Not a status the clients retry, so a miss ends the call at once instead of costing a backoff."""

MAX_DIFF_LINES = 200


class Latency(StrEnum):
    ZERO = "zero"
    """Answer at once: what is left is the agent's own time."""
    RECORDED = "recorded"
    """Wait as long as the provider took when the response was recorded."""


_WEEKDAY = r"(?:Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)"
_MONTH = r"(?:January|February|March|April|May|June|July|August|September|October|November|December)"

CANONICAL_RULES: tuple[tuple[re.Pattern[str], str], ...] = (
    # Fixture sites are served on fixed ports, but a port is never what tells two questions apart.
    (re.compile(r"(?<=//127\.0\.0\.1:)\d{1,5}|(?<=//localhost:)\d{1,5}|(?<=//\[::1\]:)\d{1,5}"), "<port>"),
    (re.compile(r"(?<=\b127\.0\.0\.1:)\d{1,5}|(?<=\blocalhost:)\d{1,5}"), "<port>"),
    # `Observation.today`: the capture date, its weekday and the two months after it.
    (
        re.compile(
            rf"\d{{4}}-\d{{2}}-\d{{2}} \({_WEEKDAY}\)\. In one month: {_MONTH} \d{{4}}; in two months: {_MONTH} \d{{4}}"
        ),
        "<today>",
    ),
    # `page.next_days`: the week after the capture date.
    (re.compile(rf"\b{_WEEKDAY} \d{{4}}-\d{{2}}-\d{{2}}\b"), "<next day>"),
    # A capture time, should one reach a prompt.
    (re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b"), "<timestamp>"),
)
"""What varies between two runs that ask the same question, as the text that stands for it in a key."""


EMBEDDED_JSON = "<json>"
"""Marks a string that held JSON, so a prompt carrying a JSON document is keyed on the document, not its spelling."""


def canonical_text(text: str) -> str:
    for pattern, placeholder in CANONICAL_RULES:
        text = pattern.sub(placeholder, text)
    return text


def _embedded(text: str) -> Any | None:
    """The JSON document a string holds, when the whole string is one object or array."""
    stripped = text.strip()
    if not (stripped.startswith("{") and stripped.endswith("}")) and not (
        stripped.startswith("[") and stripped.endswith("]")
    ):
        return None
    try:
        return json.loads(stripped)
    except ValueError:
        return None


def canonical_value(value: Any) -> Any:
    """A JSON value as it is keyed.

    Every string passes through the rules, a string holding a JSON document is replaced by that document in
    canonical form, and an integral float is written as an integer (Python writes 1.0 where serde may write 1, and
    both are one number on the wire). List order is kept: an implementation that writes a set in another order
    sends the model another prompt, and that is a miss.
    """
    if isinstance(value, str):
        if value.startswith("data:image/"):
            # A screenshot differs in its pixels from run to run (antialiasing, a caret, a frame mid-paint), and
            # the text beside it already says which page it shows.
            return "<image>"
        document = _embedded(value)
        if document is not None:
            return {EMBEDDED_JSON: canonical_value(document)}
        return canonical_text(value)
    if isinstance(value, bool) or value is None or isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value) if value.is_integer() else value
    if isinstance(value, list):
        return [canonical_value(item) for item in value]
    if isinstance(value, dict):
        return {canonical_text(str(key)): canonical_value(item) for key, item in value.items()}
    raise TypeError(f"not a JSON value: {type(value).__name__}")


TARGET_ID = re.compile(r"\b[0-9A-F]{32}\b")
"""A Chrome target id. Each browser makes up its own, and the agent offers tabs to Jev by id, so the ids are
numbered by first appearance in the body as the agent wrote it, before members are sorted (sorting first would
number them by the random ids themselves), and swapped back into the response (`translate_targets`)."""


def _as_written(body: bytes | str | Any) -> str:
    if isinstance(body, bytes):
        return body.decode("utf-8", "replace")
    return body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)


def target_ids(body: bytes | str | Any) -> list[str]:
    """The Chrome target ids a request names, in the order it first names them."""
    return list(dict.fromkeys(TARGET_ID.findall(_as_written(body))))


def canonical_json(body: bytes | str | Any) -> str:
    """The body as one canonical JSON string. A body that is not JSON is canonicalized as text."""
    text = _as_written(body)
    numbers = {target: f"<target {n}>" for n, target in enumerate(target_ids(text), 1)}
    if numbers:
        text = TARGET_ID.sub(lambda match: numbers[match.group()], text)
    try:
        parsed = json.loads(text)
    except ValueError:
        return canonical_text(text)
    return json.dumps(canonical_value(parsed), sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def translate_targets(response: str, recorded: Any, requested: Any) -> str:
    """A recorded response with the target ids of its request replaced by the ones the new request named."""
    swaps = dict(zip(target_ids(recorded), target_ids(requested), strict=False))
    if not swaps:
        return response
    return TARGET_ID.sub(lambda match: swaps.get(match.group(), match.group()), response)


def request_key(path: str, body: bytes | str | Any) -> str:
    return hashlib.sha256(f"{path}\n{canonical_json(body)}".encode()).hexdigest()


def describe_request(path: str, body: Any) -> tuple[str, str | None]:
    """The purpose and model of a request, for a person reading a miss: the schema name for the LLM, the
    question ids for Jev."""
    if not isinstance(body, dict):
        return "unknown", None
    model = body.get("model") if isinstance(body.get("model"), str) else None
    if path.startswith(LLM_MOUNT):
        schema = body.get("response_format", {})
        name = schema.get("json_schema", {}).get("name") if isinstance(schema, dict) else None
        return f"llm:{name or 'unknown'}", model
    questions = body.get("questions")
    names = sorted(questions) if isinstance(questions, dict) else []
    if isinstance(questions, list):
        names = sorted(str(q.get("id") or q.get("name") or "?") for q in questions if isinstance(q, dict))
    return f"jev:{','.join(names)[:120]}", model


def _pretty(body: Any) -> list[str]:
    keyed = canonical_json(body)
    try:
        return json.dumps(json.loads(keyed), sort_keys=True, indent=1, ensure_ascii=False).splitlines()
    except ValueError:
        return keyed.splitlines()


def miss_diff(body: Any, nearest: Any | None, *, limit: int = MAX_DIFF_LINES) -> str:
    """A unified diff from the nearest recorded body to the one that missed, cut at `limit` lines."""
    if nearest is None:
        return "(nothing recorded on this path)"
    lines = list(difflib.unified_diff(_pretty(nearest), _pretty(body), "recorded", "requested", n=2, lineterm=""))
    if len(lines) > limit:
        lines = [*lines[:limit], f"... {len(lines) - limit} more lines"]
    return "\n".join(lines)


def nearest_body(body: Any, candidates: Iterable[Any]) -> Any | None:
    """The candidate most like `body`, line by line. Ties keep the earliest, so the choice is stable."""
    target = _pretty(body)
    best, best_ratio = None, -1.0
    for candidate in candidates:
        matcher = difflib.SequenceMatcher(None, _pretty(candidate), target, autojunk=False)
        if matcher.real_quick_ratio() <= best_ratio or matcher.quick_ratio() <= best_ratio:
            continue
        ratio = matcher.ratio()
        if ratio > best_ratio:
            best, best_ratio = candidate, ratio
    return best


@dataclass
class Exchange:
    """One recorded request, as the agent sent it, and the response it got.

    The key is worked out again from the request on load, so a change to the canonical rules rekeys old cassettes.
    """

    key: str
    path: str
    purpose: str
    model: str | None
    request: Any
    status: int
    content_type: str
    body: str
    latency: float
    cost: float | None = None

    def to_json(self) -> dict[str, Any]:
        return {
            "key": self.key,
            "path": self.path,
            "purpose": self.purpose,
            "model": self.model,
            "status": self.status,
            "content_type": self.content_type,
            "latency": round(self.latency, 4),
            "cost": self.cost,
            "request": self.request,
            "body": self.body,
        }

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Self:
        return cls(
            key=request_key(data["path"], data["request"]),
            path=data["path"],
            purpose=data["purpose"],
            model=data.get("model"),
            request=data["request"],
            status=data["status"],
            content_type=data["content_type"],
            body=data["body"],
            latency=data["latency"],
            cost=data.get("cost"),
        )


@dataclass
class Miss:
    path: str
    purpose: str
    model: str | None
    key: str
    diff: str

    def to_json(self) -> dict[str, Any]:
        return {"path": self.path, "purpose": self.purpose, "model": self.model, "key": self.key, "diff": self.diff}


@dataclass
class Cassette:
    """Every exchange of one task, in the order the responses arrived."""

    task: str
    exchanges: list[Exchange] = field(default_factory=list[Exchange])
    meta: dict[str, Any] = field(default_factory=dict[str, Any])

    def save(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        head = {"cassette": CASSETTE_VERSION, "task": self.task, **self.meta}
        lines = [json.dumps(head, sort_keys=True)]
        lines.extend(json.dumps(exchange.to_json(), ensure_ascii=False) for exchange in self.exchanges)
        # mtime 0 and no file name, so recording the same exchanges twice writes the same bytes.
        with (
            path.open("wb") as raw,
            gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0, compresslevel=9) as out,
        ):
            out.write(("\n".join(lines) + "\n").encode())

    @classmethod
    def load(cls, path: Path) -> Self:
        with gzip.open(path, "rt", encoding="utf-8") as lines:
            head = json.loads(next(lines))
            if head.get("cassette") != CASSETTE_VERSION:
                raise ValueError(f"{path}: cassette version {head.get('cassette')!r}, expected {CASSETTE_VERSION}")
            exchanges = [Exchange.from_json(json.loads(line)) for line in lines if line.strip()]
        task = head.pop("task")
        head.pop("cassette")
        return cls(task=task, exchanges=exchanges, meta=head)

    @property
    def cost(self) -> float:
        return sum(exchange.cost or 0.0 for exchange in self.exchanges)


def response_cost(body: str) -> float | None:
    """The dollars OpenRouter reported for a response, for Jev and the LLM alike."""
    try:
        usage = json.loads(body).get("usage")
        cost = usage.get("cost") if isinstance(usage, dict) else None
    except (ValueError, AttributeError):
        return None
    return float(cost) if isinstance(cost, int | float) and not isinstance(cost, bool) else None


type Forward = Callable[[str, bytes], tuple[int, str, str]]
"""Send a request upstream: (path, body) to (status, content type, body)."""


def openrouter_forward(api_key: str, *, timeout: float = 120.0) -> Forward:
    """Forward to OpenRouter as a run keyed only with OPENROUTER_API_KEY would call it.

    A run pointed at a custom LLM endpoint leaves out OpenRouter's provider routing, which a normal run sends, so it
    is put back here: the recorded answers then come from the endpoints a normal run is routed to. Usage accounting
    is asked for so every response carries its cost; the key is computed from the body the agent sent.
    """
    client = httpx.Client(timeout=timeout)

    def forward(path: str, body: bytes) -> tuple[int, str, str]:
        mount = next(m for m in UPSTREAMS if path.startswith(m + "/"))
        url = UPSTREAMS[mount] + path[len(mount) :]
        payload = json.loads(body)
        if mount == LLM_MOUNT and isinstance(payload, dict):
            payload.setdefault("provider", OPENROUTER_PROVIDER_ROUTING)
            payload.setdefault("usage", {"include": True})
        try:
            response = client.post(url, json=payload, headers={"Authorization": f"Bearer {api_key}"})
        except httpx.HTTPError as error:
            return 502, "application/json", json.dumps({"error": {"message": f"upstream: {type(error).__name__}"}})
        return response.status_code, response.headers.get("content-type", "application/json"), response.text

    return forward


class Proxy:
    """The local HTTP server the agent's model clients talk to, for one cassette.

    With `forward` and not `replay` it records: every request goes upstream. With `replay` and no `forward` it
    replays, and a request it has no response for is a miss. With both it fills: what it has is served, and a miss
    is sent upstream and added to the cassette. Filling at zero latency records the path a run takes when every
    answer is instant, which a live recording cannot (see `bench.py` on the plan race).
    """

    def __init__(
        self,
        cassette: Cassette,
        *,
        forward: Forward | None = None,
        replay: bool = True,
        latency: Latency = Latency.ZERO,
        on_miss: Callable[[Miss], None] | None = None,
    ) -> None:
        if forward is None and not replay:
            raise ValueError("a proxy that neither replays nor forwards has nothing to answer with")
        self.cassette = cassette
        self._forward = forward
        self._replay = replay
        self._latency = latency
        self._on_miss = on_miss
        self._lock = threading.Lock()
        self._served: dict[str, int] = {}
        self._by_key: dict[str, list[Exchange]] = {}
        if replay:
            for exchange in cassette.exchanges:
                self._by_key.setdefault(exchange.key, []).append(exchange)
        self.misses: list[Miss] = []
        self.requests = 0
        self.forwarded = 0
        self._server: ThreadingHTTPServer | None = None

    def answer(self, path: str, raw: bytes) -> tuple[int, str, str, float]:
        """The response to one request, and how long to hold it before sending."""
        try:
            body: Any = json.loads(raw)
        except ValueError:
            body = raw.decode("utf-8", "replace")
        key = request_key(path, body)
        purpose, model = describe_request(path, body)
        with self._lock:
            self.requests += 1
            stored = self._by_key.get(key) if self._replay else None
            if stored:
                index = self._served.get(key, 0)
                self._served[key] = index + 1
                exchange = stored[min(index, len(stored) - 1)]
                hold = exchange.latency if self._latency is Latency.RECORDED else 0.0
                text = translate_targets(exchange.body, exchange.request, body)
                return exchange.status, exchange.content_type, text, hold
        if self._replay:
            nearest = nearest_body(body, (e.request for e in self.cassette.exchanges if e.path == path))
            miss = Miss(path=path, purpose=purpose, model=model, key=key, diff=miss_diff(body, nearest))
            with self._lock:
                self.misses.append(miss)
            if self._on_miss is not None:
                self._on_miss(miss)
        if self._forward is None:
            message = f"replay miss: no recorded response for {purpose} ({model}) on {path}"
            return MISS_STATUS, "application/json", json.dumps({"error": {"message": message}}), 0.0
        started = time.monotonic()
        status, content_type, text = self._forward(path, raw)
        exchange = Exchange(
            key=key,
            path=path,
            purpose=purpose,
            model=model,
            request=body,
            status=status,
            content_type=content_type,
            body=text,
            latency=time.monotonic() - started,
            cost=response_cost(text),
        )
        with self._lock:
            self.forwarded += 1
            self.cassette.exchanges.append(exchange)
            if self._replay:
                self._by_key.setdefault(key, []).append(exchange)
                self._served[key] = self._served.get(key, 0) + 1
        return status, content_type, text, 0.0

    def unused(self) -> int:
        """Recorded responses no request asked for: the other half of a miss, or a hedge that never fired."""
        return sum(max(0, len(stored) - self._served.get(key, 0)) for key, stored in self._by_key.items())

    def __enter__(self) -> str:
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length)
                status, content_type, text, hold = proxy.answer(self.path, raw)
                if hold:
                    time.sleep(hold)
                payload = text.encode()
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_GET(self) -> None:
                # Receipts and price lists are looked up only on OpenRouter's own host, so a GET here is new.
                status, content_type, text, _ = proxy.answer(self.path, b"")
                payload = text.encode()
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request: Any, client_address: Any) -> None:
                # The agent drops the losing twin of a hedged call; its closed socket is expected, and the
                # response it would have read is recorded already.
                if not isinstance(sys.exception(), ConnectionError):
                    super().handle_error(request, client_address)

        self._server = Server(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._server.serve_forever, name="fastbrowse-replay", daemon=True).start()
        return f"http://127.0.0.1:{self._server.server_port}"

    def __exit__(self, *exc: object) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
