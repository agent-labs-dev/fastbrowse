"""The messages `fastbrowse serve` exchanges with its client, and the version of that exchange.

JSON-RPC 2.0, one message per line. Every shape that crosses the pipe is a model here, so the server and the
types generated for the JavaScript SDK are read from one definition.
"""

from enum import IntEnum, StrEnum
from pathlib import Path
from typing import Any, Literal

from pydantic import Base64Bytes, BaseModel, ConfigDict, Field, StrictInt, StrictStr

from fastbrowse.models import Authorization, BrowserEvent, Limits, LocalChrome, SecretRef, StepEvent

# Changes only when the wire format does, so it is not the package version: a release that leaves the
# messages alone leaves this alone, and an SDK can tell a binary it cannot talk to from one that is merely newer.
PROTOCOL_VERSION = 1

# Strict, because JSON-RPC echoes the id back and a `true` read as 1 would answer a request nobody sent.
type RequestId = StrictInt | StrictStr


class Method(StrEnum):
    """What a client can ask of the server."""

    INITIALIZE = "initialize"
    RUN = "run"
    RUN_CANCEL = "run/cancel"
    SHUTDOWN = "shutdown"


class ServerMethod(StrEnum):
    """What the server sends without being asked."""

    RUN_EVENT = "run/event"
    SECRETS_RESOLVE = "secrets/resolve"


class ErrorCode(IntEnum):
    """Every code an error reply can carry: the JSON-RPC standard ones, then this protocol's own."""

    PARSE_ERROR = -32700
    INVALID_REQUEST = -32600
    METHOD_NOT_FOUND = -32601
    INVALID_PARAMS = -32602
    INTERNAL_ERROR = -32603
    # JSON-RPC reserves -32000 to -32099 for the server's own errors.
    BUSY = -32001
    CANCELLED = -32002
    CONFIGURATION = -32003
    UNSUPPORTED_SCHEMA = -32004


class Request(BaseModel):
    """A call in either direction. One without an id is a notification and gets no reply."""

    jsonrpc: Literal["2.0"] = "2.0"
    id: RequestId | None = None
    method: str
    # JSON-RPC also allows params by position. No method here takes them, and accepting the shape lets the
    # refusal be "invalid params" for that method instead of "invalid request".
    params: dict[str, Any] | list[Any] | None = None


class Response(BaseModel):
    jsonrpc: Literal["2.0"] = "2.0"
    id: RequestId
    result: Any


class Error(BaseModel):
    code: ErrorCode
    message: str


class ErrorResponse(BaseModel):
    jsonrpc: Literal["2.0"] = "2.0"
    # Null when the request could not be read far enough to learn its id.
    id: RequestId | None
    error: Error


class Params(BaseModel):
    """A method's params. An unknown field is refused, so a misspelled option is never dropped in silence."""

    model_config = ConfigDict(extra="forbid")


class NoParams(Params):
    pass


class InitializeParams(BaseModel):
    """The client's side of the handshake.

    Unknown fields are allowed here and nowhere else: a client on a later protocol may send some, and it has to
    get the server's version back to report the mismatch, which a refusal of its params would hide.
    """

    protocol_version: StrictInt


class InitializeResult(BaseModel):
    protocol_version: int
    fastbrowse_version: str


class RunAttachment(Params):
    """An `Attachment` as it travels: JSON has no bytes, so the content is base64."""

    name: str
    mime_type: str
    content: Base64Bytes


class RunParams(Params):
    """One run. Apart from `run_id`, each field is the `run_task` argument or the CLI flag of the same name.

    The reply is the `RunResult`. No API key is among the fields: model and browser keys come from the
    environment the server inherited.
    """

    run_id: str = Field(min_length=1)
    """Chosen by the client, and carried by every message the server sends about this run."""
    task: str
    start: str | None = None
    inputs: dict[str, str] | None = None
    attachments: tuple[RunAttachment, ...] = ()
    limits: Limits | None = None
    authorization: Authorization | None = None
    output_schema: dict[str, Any] | None = None
    """The shape of `RunResult.data`, as JSON Schema draft 2020-12 with an object at its root."""
    secrets: tuple[SecretRef, ...] = ()
    """The secrets the client holds, by name and origin. A value is asked for with `secrets/resolve`."""
    downloads: Path | None = None
    record: Path | None = None
    local: bool = False
    """Local Chrome instead of a Browser Use Cloud browser."""
    chrome: LocalChrome | None = None
    """Which local Chrome, and how. A headed window or a kept profile implies `local`."""
    cloud_profile: str | None = None
    cdp_url: str | None = None
    cdp_port: int | None = None
    attach: bool = False
    target_match: str | None = None
    proxy_country: str | None = None
    viewport: tuple[int, int] | None = None
    cloud_allow_resizing: bool = False


class RunCancelParams(Params):
    """Which run to stop. One that is not active, because it finished or never was, is left as it is.

    The reply says only that the request was read. The run's own reply is what says it was cancelled: the
    `cancelled` error, written after its browser has closed.
    """

    run_id: str


class RunEvent(BaseModel):
    """The params of `run/event`: one thing a run did, written before that run's reply."""

    run_id: str
    event: StepEvent | BrowserEvent


class Notification(BaseModel):
    """A message from the server that takes no reply. It has no `id` member at all, which is what marks it."""

    jsonrpc: Literal["2.0"] = "2.0"
    method: ServerMethod
    params: Any


class ServerRequest(BaseModel):
    """A call from the server that the client answers, with a `Response` or an `ErrorResponse` carrying its id.

    The ids are the server's own and count up from one. A client's ids may be the same numbers: a reply is
    told from a request by having no `method`, so the two never meet.
    """

    jsonrpc: Literal["2.0"] = "2.0"
    id: int
    method: ServerMethod
    params: Any


class SecretsResolveParams(BaseModel):
    """The params of `secrets/resolve`: one value the run is about to type. The reply is the value, or null."""

    run_id: str
    name: str
    origin: str
    """Where it will be typed. The server has already checked it against the origins the ref declared."""
