"""The keys and model backend every fastbrowse arm runs with, chosen here and passed to `run_task(settings=...)`.

fastbrowse reads its configuration from the process environment and `.env` unless the caller passes `settings`, in
which case it reads those alone. The harness passes them, so the Jev route is one Parallax chose and recorded. The
environment is read as fastbrowse reads it (`environment`).
"""

from collections.abc import Mapping
from typing import Literal

import httpx
from fastbrowse.evals import harness

type JevSource = Literal["openrouter", "typesafe"]

RETRYABLE_STATUS = frozenset({408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529})
"""Statuses that say nothing about the request, so a repeat can clear them (crates/fastbrowse-clients/src/validation.rs
`RETRYABLE_STATUS`)."""
TRANSIENT_TRANSPORT: tuple[type[httpx.HTTPError], ...] = (
    httpx.TimeoutException,
    httpx.NetworkError,
    httpx.RemoteProtocolError,
    httpx.ProxyError,
    httpx.DecodingError,
)
"""Transport failures a repeat can clear; fastbrowse's own Python client used exactly these before its Rust core."""

PROVIDER_KEYS = ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "EXA_API_KEY")
"""The provider keys a fastbrowse arm is given: Jev, its LLM and optional web search use these.
The Browser Use key stays with the harness, which starts every cloud browser itself."""


class MissingKey(RuntimeError):
    """A key the selected arms need is not set."""


class NotStarted(RuntimeError):
    """The provider refused the request that starts a run with a status a repeat cannot clear (401 for a bad key, say),
    so it created nothing and billed nothing: the attempt's budget hold is released rather than kept."""


def refusal(status: int | None) -> bool:
    """Whether a provider's answer to a start request refused it outright: an error status a repeat cannot clear."""
    return status is not None and status >= 400 and status not in RETRYABLE_STATUS


def environment() -> dict[str, str]:
    """The process environment and `.env`, read as fastbrowse reads them; the one place the harness reads keys."""
    return harness.environment()


def browser_key() -> str:
    if key := environment().get("BROWSER_USE_API_KEY"):
        return key
    raise MissingKey("set BROWSER_USE_API_KEY for a Browser Use Cloud browser or the hosted Browser Use agent")


def openrouter_key() -> str:
    if key := environment().get("OPENROUTER_API_KEY"):
        return key
    raise MissingKey("set OPENROUTER_API_KEY")


def jev_source(env: Mapping[str, str] | None = None) -> JevSource:
    """The Jev provider every arm that calls Jev uses: the one named in FASTBROWSE_DECISION_SOURCE (or its former
    name FASTBROWSE_JEV_SOURCE), else the direct API when it alone is keyed, else OpenRouter."""
    env = environment() if env is None else env
    named = env.get("FASTBROWSE_DECISION_SOURCE") or env.get("FASTBROWSE_JEV_SOURCE")
    if named is not None:
        if named not in ("openrouter", "typesafe"):
            raise MissingKey(f"FASTBROWSE_DECISION_SOURCE={named} serves no Jev; use openrouter or typesafe")
        return named
    if env.get("OPENROUTER_API_KEY"):
        return "openrouter"
    if env.get("TYPESAFE_API_KEY"):
        return "typesafe"
    return "openrouter"


def providers() -> str:
    """The Jev route a run records as its providers, never a key."""
    return f"jev {jev_source()}"


def run_settings(overrides: Mapping[str, str] | None = None) -> dict[str, str]:
    """`run_task(settings=...)` for a fastbrowse arm: the provider keys and fastbrowse's own options from the
    environment, with the Jev source pinned to `jev_source()`."""
    env = environment()
    chosen = {name: value for name, value in env.items() if name in PROVIDER_KEYS or name.startswith("FASTBROWSE_")}
    chosen.pop("FASTBROWSE_JEV_SOURCE", None)
    if overrides is not None:
        chosen = {
            name: value
            for name, value in chosen.items()
            if not name.endswith("API_KEY") and not name.startswith("FASTBROWSE_JEV_")
        }
        return chosen | dict(overrides)
    return chosen | {"FASTBROWSE_DECISION_SOURCE": jev_source(env)}
