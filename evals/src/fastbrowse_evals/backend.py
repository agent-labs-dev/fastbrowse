"""The keys and model backend every fastbrowse arm runs with, chosen here and passed to `run_task(settings=...)`.

fastbrowse reads its configuration from the process environment and `.env` unless the caller passes `settings`, in
which case it reads those alone. The harness passes them, so the Jev route is one Parallax chose and recorded, and
jev-ultrafast, which cannot import fastbrowse, is given the same route by construction rather than by agreeing on an
environment. The environment is read as fastbrowse reads it (`environment`).
"""

import importlib
from collections.abc import Mapping
from typing import Any, Literal

import httpx
from fastbrowse.evals import harness

type JevSource = Literal["openrouter", "typesafe", "gateway"]

# Values of the pinned fastbrowse build's Rust core: crates/fastbrowse-clients/src/system_one.rs and vercel.rs.
OPENROUTER_URL = "https://openrouter.ai/api"
TYPESAFE_URL = "https://api.typesafe.ai"
GATEWAY_URL = "https://ai-gateway.vercel.sh"
JEV_DOLLARS_PER_INPUT_TOKEN = 0.042 / 1_000_000
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

PROVIDER_KEYS = ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "EXA_API_KEY")
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
    name FASTBROWSE_JEV_SOURCE), else OpenRouter when keyed, else the direct API when keyed, else the gateway."""
    env = environment() if env is None else env
    named = env.get("FASTBROWSE_DECISION_SOURCE") or env.get("FASTBROWSE_JEV_SOURCE")
    if named is not None:
        if named not in ("openrouter", "typesafe", "gateway"):
            raise MissingKey(f"FASTBROWSE_DECISION_SOURCE={named} serves no Jev; use openrouter, typesafe or gateway")
        return named
    if env.get("OPENROUTER_API_KEY"):
        return "openrouter"
    return "typesafe" if env.get("TYPESAFE_API_KEY") else "gateway"


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


def jev_models() -> dict[str, str]:
    """The Jev model each source sends unless a run names another, as the measured fastbrowse build's Rust core
    defines it, so jev-ultrafast runs the Jev release fastbrowse runs and no copy here can fall behind."""
    core: Any = importlib.import_module("fastbrowse._native")  # compiled, so the checker has no types for it
    return dict(core.jev_models())


def jev_route() -> tuple[str, str, str, str | None]:
    """(key variable, base URL, model, key) of `jev_source()`, for jev-ultrafast's isolated runner. A base URL or
    model set for fastbrowse (FASTBROWSE_DECISION_BASE_URL, FASTBROWSE_DECISION_MODEL) applies to it too."""
    env = environment()
    source = jev_source(env)
    match source:
        case "openrouter":
            name, url, key = "TYPESAFE_API_KEY", OPENROUTER_URL, env.get("OPENROUTER_API_KEY")
        case "typesafe":
            name, url, key = "TYPESAFE_API_KEY", TYPESAFE_URL, env.get("TYPESAFE_API_KEY")
        case "gateway":
            name, url, key = "AI_GATEWAY_API_KEY", GATEWAY_URL, env.get("AI_GATEWAY_API_KEY")
    model = jev_models()[source]
    url = env.get("FASTBROWSE_DECISION_BASE_URL") or env.get("FASTBROWSE_JEV_BASE_URL") or url
    model = env.get("FASTBROWSE_DECISION_MODEL") or env.get("FASTBROWSE_JEV_MODEL") or model
    return name, url, model, key
