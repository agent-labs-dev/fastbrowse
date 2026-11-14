"""The keys and model backend every fastbrowse arm runs with, chosen here and passed to `run_task(settings=...)`.

fastbrowse reads its configuration from the process environment and `.env` unless the caller passes `settings`, in
which case it reads those alone. The harness passes them, so the Jev route is one Parallax chose and recorded. The
environment is read as fastbrowse reads it (`environment`).
"""

import os
import time
from collections.abc import AsyncIterator, Iterable, Mapping
from contextlib import asynccontextmanager
from typing import assert_never

import httpx
from fastbrowse.evals import harness
from fastbrowse.models import Authorization, Citation, Limits, RunResult, Status
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of
from parallax.harnesses.browser import BrowserHarness
from parallax.harnesses.browser.cloud import Browser, cloud_browser
from parallax.models import Capability, CostSource, HarnessInfo, HarnessKind, Isolation, Task, TurnInput

from fastbrowse_evals.task_specs import answer_model

PROVIDER_KEYS = ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "EXA_API_KEY")
"""The provider keys a fastbrowse arm is given: Jev, its LLM and optional web search use these.
The Browser Use key stays with the harness, which starts every cloud browser itself."""

OPENROUTER_KEY_ENV = "FASTBROWSE_OPENROUTER_API_KEY"
"""Where the arm's OpenRouter key comes from. Parallax keeps `OPENROUTER_API_KEY` for its meter, which forwards only
chat and responses calls; fastbrowse also calls Jev's `/v1/systemone`, so the arm cannot be metered, reports its own
spend, and takes its key under its own name. fastbrowse itself still reads it as `OPENROUTER_API_KEY`."""


class MissingKey(RuntimeError):
    """A key the selected arms need is not set."""


def environment() -> dict[str, str]:
    """The process environment and `.env`, read as fastbrowse reads them; the one place the harness reads keys.

    The arm's OpenRouter key is `OPENROUTER_KEY_ENV` alone: a plain `OPENROUTER_API_KEY` beside it is ignored."""
    env = harness.environment()
    env.pop("OPENROUTER_API_KEY", None)
    if key := env.pop(OPENROUTER_KEY_ENV, None):
        env["OPENROUTER_API_KEY"] = key
    return env


def jev_source(env: Mapping[str, str] | None = None) -> str:
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
        capabilities=frozenset({Capability.BROWSER, Capability.CREDENTIALS, Capability.AUTHORIZATION}),
        pins_model=False,
        cost_source=CostSource.HARNESS,
        isolation=Isolation.HOST,
        credential_env=(OPENROUTER_KEY_ENV, "TYPESAFE_API_KEY", "EXA_API_KEY"),
        source_url="https://github.com/agent-labs-dev/fastbrowse",
    )

    def __init__(self, *, observer: str | None = None) -> None:
        super().__init__(observer=observer)
        self._task: Task | None = None
        self._product_status: Status | None = None

    def bind_task(self, task: Task) -> None:
        super().bind_task(task)
        self._task = task

    @asynccontextmanager
    async def browser(self, http: httpx.AsyncClient) -> AsyncIterator[Browser]:
        async with cloud_browser(http, screen=(1120, 780)) as browser:
            yield browser

    async def execute(self, turn: TurnInput) -> dict[str, object]:
        task = self._task
        if task is None:
            raise RuntimeError("fastbrowse harness has no task")
        start_url = task.data.get("start_url")
        if not isinstance(start_url, str):
            raise ValueError("browser task has no start_url")
        # The runner made the trial INVALID before this point if any of these variables is unset.
        values = {alias: os.environ[env_name] for alias, env_name in task.secrets.items()}
        secrets = ScopedSecrets(values, origin_of(start_url)) if values else None
        authorization = Authorization(irreversible_actions=Capability.AUTHORIZATION in task.meta.requires)
        async with httpx.AsyncClient() as http, self.browser(http) as browser:
            self._browser = browser
            self._details["browser"] = "cloud" if browser.browser_id else "pool"
            self._agent_started(browser.open_seconds)
            began = time.perf_counter()
            output = await run_task(
                turn.text,
                start=start_url,
                open_browser=lambda: _ready_browser(browser),
                output_schema=answer_model(task.id),
                limits=Limits(),
                authorization=authorization,
                secrets=secrets,
                http=http,
                settings=run_settings(),
            )
            self._product_status = output.status
            latency_ms = round((time.perf_counter() - began) * 1000)
            page = await self.observe(browser.cdp_url, url=output.final_url)
        return _run_result(output, page, browser, latency_ms=latency_ms)

    def trial_details(self) -> dict[str, object]:
        details = super().trial_details()
        if self._product_status is not None:
            details["status"] = self._product_status.value
        return details


async def _ready_browser(browser: Browser) -> str:
    return browser.cdp_url


def _turn_status(status: Status) -> str:
    match status:
        case Status.UNAVAILABLE:
            return "unavailable"
        case Status.ERROR:
            return "error"
        case (
            Status.COMPLETE
            | Status.UNVERIFIED
            | Status.NEEDS_CONFIRMATION
            | Status.NEEDS_LOGIN
            | Status.BLOCKED
            | Status.NEEDS_INPUT
            | Status.STUCK
            | Status.BUDGET_EXCEEDED
            | Status.OBSERVATION_LIMIT
            | Status.CANCELLED
        ):
            return "done"
        case other:
            assert_never(other)


def evidence_citations(citations: Iterable[Citation]) -> list[dict[str, str]]:
    """Each citation as Parallax's browser evidence holds it: the page and the quote the answer rests on."""
    return [{"url": citation.url, "quote": citation.quote} for citation in citations]


def _run_result(output: RunResult, page: dict[str, object], browser: Browser, *, latency_ms: int) -> dict[str, object]:
    lines = [line.model_dump(mode="json") for line in output.cost.lines]
    lines.extend(browser.cost)
    usage = []
    for line in output.cost.lines:
        component = line.component.value
        if component not in {"llm", "jev"}:
            continue
        label = line.label
        model = label.partition(":")[2] or label
        usage.append(
            {
                "model": model,
                "component": component,
                "input": line.input_tokens,
                "output": line.output_tokens,
                "requests": line.physical_requests or 0,
                "cost_usd": line.dollars,
                "source": "harness",
            }
        )
    steps = []
    for step in output.steps:
        item = {"action": step.operation.value, "url": step.url}
        if step.target:
            item["value"] = step.target
        if step.facts:
            item["quote"] = step.facts[0].quote or step.facts[0].text
        steps.append(item)
    return {
        "status": _turn_status(output.status),
        "answer": output.answer,
        "error": output.error,
        "final_page": page,
        "final_url": output.final_url,
        "step_log": steps,
        "citations": evidence_citations(output.citations),
        "cost_lines": lines,
        "usage": usage,
        "latency_ms": latency_ms,
        "actual_model": None,
    }
