"""The messages `fastbrowse serve` exchanges with its client, and the version of that exchange.

JSON-RPC 2.0, one message per line. Every shape that crosses the pipe is a model here, so the server and the
types generated for the JavaScript SDK are read from one definition.
"""

from enum import IntEnum, StrEnum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, StrictInt, StrictStr

# Changes only when the wire format does, so it is not the package version: a release that leaves the
# messages alone leaves this alone, and an SDK can tell a binary it cannot talk to from one that is merely newer.
PROTOCOL_VERSION = 1

# Strict, because JSON-RPC echoes the id back and a `true` read as 1 would answer a request nobody sent.
type RequestId = StrictInt | StrictStr


class Method(StrEnum):
    INITIALIZE = "initialize"
    SHUTDOWN = "shutdown"


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
