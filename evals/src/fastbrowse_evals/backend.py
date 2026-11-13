"""The keys and model backend every fastbrowse arm runs with, chosen here and passed to `run_task(settings=...)`.

fastbrowse reads its configuration from the process environment and `.env` unless the caller passes `settings`, in
which case it reads those alone. The harness passes them, so the Jev route is one Parallax chose and recorded. The
environment is read as fastbrowse reads it (`environment`).
"""

from collections.abc import AsyncIterator, Mapping
from contextlib import asynccontextmanager
from typing import Literal

import httpx
from fastbrowse.evals import harness
from fastbrowse.models import Authorization, Limits
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of
from parallax.harnesses.browser import BrowserHarness
from parallax.harnesses.browser.cloud import Browser, cloud_browser
from parallax.models import Capability, CostSource, HarnessInfo, HarnessKind, Isolation, TurnInput

from fastbrowse_evals.task_specs import secret_values

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


class FastbrowseHarness(BrowserHarness):
    """Run the public fastbrowse embedder inside Parallax's browser protocol."""

    info = HarnessInfo(
        name="fastbrowse",
        kind=HarnessKind.OPEN,
        capabilities=frozenset({Capability.BROWSER}),
        pins_model=False,
        cost_source=CostSource.HARNESS,
        isolation=Isolation.HOST,
        credential_env=("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "EXA_API_KEY"),
        source_url="https://github.com/agent-labs-dev/fastbrowse",
    )

    def __init__(self, *, observer: str | None = None) -> None:
        super().__init__(observer=observer)
        self._task_data: Mapping[str, object] = {}

    def bind_task(self, task: object) -> None:
        data = getattr(task, "data", {})
        self._task_data = data if isinstance(data, Mapping) else {}

    @asynccontextmanager
    async def browser(self, http: httpx.AsyncClient) -> AsyncIterator[Browser]:
        async with cloud_browser(http, screen=(1120, 780)) as browser:
            yield browser

    async def execute(self, turn: TurnInput) -> dict[str, object]:
        start_url = self._task_data.get("start_url")
        if not isinstance(start_url, str):
            raise ValueError("browser task has no start_url")
        task_id = self.trial.task_id
        secrets = ScopedSecrets(secret_values(task_id), origin_of(start_url)) if secret_values(task_id) else None
        authorization = Authorization(irreversible_actions=bool(self._task_data.get("authorize")))
        async with httpx.AsyncClient() as http, self.browser(http) as browser:
            self._browser = browser
            self._details["browser"] = "cloud" if browser.browser_id else "pool"
            self._agent_started(browser.open_seconds)
            output = await run_task(
                turn.text,
                start=start_url,
                open_browser=lambda: _ready_browser(browser),
                output_schema=None,
                limits=Limits(),
                authorization=authorization,
                secrets=secrets,
                http=http,
                settings=run_settings(),
            )
            page = await self.observe(browser.cdp_url, url=output.final_url)
            return _run_result(output, page, browser)


async def _ready_browser(browser: Browser) -> str:
    return browser.cdp_url


def _run_result(output: object, page: dict[str, object], browser: Browser) -> dict[str, object]:
    status = getattr(getattr(output, "status", None), "value", getattr(output, "status", "error"))
    if status == "complete":
        ending = "done"
    elif status == "unavailable":
        ending = "unavailable"
    else:
        ending = "stopped"
    cost = getattr(output, "cost", None)
    lines = [line.model_dump(mode="json") for line in getattr(cost, "lines", ())]
    lines.extend(browser.cost)
    steps = []
    for step in getattr(output, "steps", ()):
        item = {"action": step.operation.value, "url": step.url}
        if step.target:
            item["value"] = step.target
        if step.facts:
            item["quote"] = step.facts[0].quote or step.facts[0].text
        steps.append(item)
    citations = [citation.model_dump(mode="json") for citation in getattr(output, "citations", ())]
    return {
        "status": ending,
        "answer": getattr(output, "answer", None),
        "error": getattr(output, "error", None),
        "final_page": page,
        "final_url": getattr(output, "final_url", None),
        "step_log": steps,
        "citations": citations,
        "cost_lines": lines,
        "actual_model": None,
    }
