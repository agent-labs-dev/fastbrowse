"""The keys and model backend every fastbrowse arm runs with, chosen here and passed to `run_task(settings=...)`.

fastbrowse reads its configuration from the process environment and `.env` unless the caller passes `settings`, in
which case it reads those alone. The harness passes them, so the Jev route is one Parallax chose and recorded. The
environment is read as fastbrowse reads it (`environment`).
"""

import hashlib
import json
import os
import tempfile
import time
from collections.abc import AsyncIterator, Iterable, Mapping
from contextlib import asynccontextmanager
from pathlib import Path
from typing import assert_never
from urllib.parse import urlparse
from urllib.request import url2pathname

import httpx
from fastbrowse.evals import harness
from fastbrowse.models import Artifact, Authorization, Citation, Limits, RunResult, Status
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets
from fastbrowse.telemetry import traced
from parallax.harnesses.browser import BrowserHarness
from parallax.harnesses.browser.cloud import Browser, browser_minutes, cloud_browser
from parallax.models import Capability, CostSource, HarnessInfo, HarnessKind, Isolation, Task, TurnInput

from fastbrowse_evals.evidence import result_answer, step_action
from fastbrowse_evals.task_specs import answer_model, secrets_origin, start_url

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
        self._events: list[object] = []
        self._assets: list[dict[str, str | None]] = []
        # One media type per content digest: an asset is named by its bytes, so the same bytes served under two
        # types are one asset, and the first type wins.
        self._asset_mimes: dict[str, str] = {}

    def bind_task(self, task: Task) -> None:
        super().bind_task(task)
        self._task = task

    @asynccontextmanager
    async def browser(self, http: httpx.AsyncClient) -> AsyncIterator[Browser]:
        # A browser whose stop is missed (the process was killed) then stops itself when its turn ends.
        async with cloud_browser(http, screen=(1120, 780), minutes=browser_minutes(self.seconds_left())) as browser:
            yield browser

    async def execute(self, turn: TurnInput) -> dict[str, object]:
        task = self._task
        if task is None:
            raise RuntimeError("fastbrowse harness has no task")
        start = start_url(task)
        # The runner made the trial INVALID before this point if any of these variables is unset.
        values = {alias: os.environ[env_name] for alias, env_name in task.secrets.items()}
        secrets = ScopedSecrets(values, secrets_origin(task)) if values else None
        authorization = Authorization(irreversible_actions=task.meta.authorizes)
        # Where the run keeps its artifacts until they are retained as trial assets.
        kept = tempfile.TemporaryDirectory(prefix="fastbrowse-artifacts-")
        async with httpx.AsyncClient() as http, self.browser(http) as browser:
            self._browser = browser
            self._details["browser"] = "cloud" if browser.browser_id else "pool"
            self._agent_started(browser.open_seconds)
            began = time.perf_counter()
            with traced() as events:
                self._events = events
                output = await run_task(
                    turn.text,
                    start=start,
                    open_browser=lambda: _ready_browser(browser),
                    output_schema=answer_model(task.id),
                    # Parallax's turn limit, less the browser's start: fastbrowse ends the run itself, with its
                    # answer and evidence, rather than being cancelled mid-step.
                    limits=Limits(max_seconds=self.seconds_left()),
                    authorization=authorization,
                    secrets=secrets,
                    http=http,
                    downloads=Path(kept.name),
                    settings=self.settings(),
                )
            # The answer is in: reading the final page and stopping the browser are not the agent's time.
            self._agent_finished()
            with kept:
                self.retain(output.artifacts)
            self._product_status = output.status
            latency_ms = round((time.perf_counter() - began) * 1000)
            # A run that read every page over HTTP never drove the browser, which still shows the blank tab it
            # opened on; what it saw is in `pages`.
            page = None if output.browser is None else await self.observe(browser.cdp_url, url=output.final_url)
        return _run_result(output, page, browser, events, latency_ms=latency_ms)

    def settings(self) -> dict[str, str]:
        return run_settings()

    def retain(self, artifacts: Iterable[Artifact]) -> None:
        """Register each artifact the run kept (the screenshot an image reading read, a download, a text capture) as
        a trial asset, and list it in the trial metadata. An asset is named by its content hash, so the
        `screenshot-<sha256>` an image reading cites in the trace is the retained `assets/<sha256>`."""
        assets = self.trial.assets
        artifacts = tuple(artifacts)
        if assets is None or not artifacts:
            return
        if self._secret_values:
            # A bound login can show in a screenshot or a download, and neither can be scrubbed as text is.
            self.trial_notes.append("run artifacts not retained: the task binds secrets")
            return
        for artifact in artifacts:
            data = Path(url2pathname(urlparse(artifact.uri).path)).read_bytes()
            # A trial asset takes the bare media type: a parameter such as `charset` is not part of it.
            mime = artifact.mime_type.partition(";")[0].strip()
            # A trial asset holds non-empty bytes: an empty download is listed in the metadata with no asset, since
            # there is nothing to replay.
            reference = (
                assets.register(data, self._asset_mimes.setdefault(hashlib.sha256(data).hexdigest(), mime))
                if data
                else None
            )
            self._assets.append({"kind": artifact.kind.value, "name": artifact.name, "asset": reference})

    def cancelled(self) -> dict[str, object]:
        """The pages and steps a run reached before Parallax's turn limit cancelled it, from its trace events: a
        cancelled `run_task` never returns the steps `_run_result` reads. The step it was in has no outcome yet, so
        it reads `cancelled`."""
        return cancelled_evidence(self._events)

    @property
    def trial_metadata(self) -> dict[str, object]:
        metadata = {"events": self._scrub(json.loads(json.dumps(self._events, default=str)))}
        if self._assets:
            metadata["assets"] = self._assets
        return super().trial_metadata | metadata

    def trial_details(self) -> dict[str, object]:
        details = super().trial_details()
        if self._product_status is not None:
            details["status"] = self._product_status.value
        return details


class FastbrowseLlmPickerHarness(FastbrowseHarness):
    """fastbrowse with its LLM picking each browser action in place of Jev: the same-model ablation of the picker.

    Every other decision, and every input the picker sees, is the `fastbrowse` arm's."""

    info = FastbrowseHarness.info.model_copy(update={"name": "fastbrowse-llm-picker"})

    def settings(self) -> dict[str, str]:
        return run_settings() | {"FASTBROWSE_ACTION_PICKER": "llm"}


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


def read_pages(events: Iterable[object]) -> tuple[list[dict[str, object]], dict[int, int]]:
    """The pages a run saw, each once and in the order it first saw them, from the `page` and `read` events of its
    trace, and for each step the index of the last page it saw. Over HTTP the browser never shows these pages, so
    the trace is the only record of what the run read."""
    pages: list[dict[str, object]] = []
    index: dict[tuple[object, object, object], int] = {}
    seen_at: dict[int, int] = {}
    for event in events:
        if not isinstance(event, Mapping) or event.get("event") not in ("page", "read"):
            continue
        # A read event's text is its capture's, kept typed beside the blocks and sources the notes were built from.
        page = event.get("capture") if event["event"] == "read" else event
        if (
            not isinstance(page, Mapping)
            or not isinstance(page.get("text"), str)
            or not isinstance(event.get("step"), int)
        ):
            continue
        record = {key: event[key] for key in ("url", "title", "controls") if event.get(key) is not None}
        record["text"] = page["text"]
        key = (record.get("url"), record.get("title"), record["text"])
        if key not in index:
            index[key] = len(pages)
            pages.append(record)
        seen_at[event["step"]] = index[key]
    return pages, seen_at


def cancelled_evidence(events: Iterable[object]) -> dict[str, object]:
    """`pages` and `step_log`, as `_run_result` reports them, rebuilt from the trace of a run that never returned."""
    events = list(events)
    pages, seen_at = read_pages(events)
    decided: dict[int, Mapping[str, object]] = {}
    for event in events:
        if (
            isinstance(event, Mapping)
            and event.get("event") == "decision_context"
            and isinstance(event.get("step"), int)
        ):
            decided[event["step"]] = (
                event  # a step decided again, as when a read moves into the browser, keeps its last
            )
    steps: list[dict[str, object]] = []
    for position, (index, decision) in enumerate(sorted(decided.items())):
        outcome = "cancelled" if position == len(decided) - 1 else None
        item: dict[str, object] = {"action": step_action(str(decision.get("operation")), outcome, None)}
        if index in seen_at:
            item["page"] = seen_at[index]
            item["url"] = pages[seen_at[index]].get("url")
        if isinstance(decision.get("target_id"), str):
            item["value"] = decision["target_id"]
        steps.append(item)
    return {"pages": pages, "step_log": steps}


def _run_result(
    output: RunResult, page: dict[str, object] | None, browser: Browser, events: Iterable[object], *, latency_ms: int
) -> dict[str, object]:
    lines = [line.model_dump(mode="json") for line in output.cost.lines]
    lines.extend(browser.cost)
    usage = []
    for line in output.cost.lines:
        component = line.component.value
        if component not in {"llm", "jev"}:
            continue
        # A campaign measures any fastbrowse_ref, and builds before the served model was recorded have no `model`.
        usage.append(
            {
                "model": getattr(line, "model", None),
                "component": component,
                "input": line.input_tokens,
                "output": line.output_tokens,
                "requests": line.physical_requests or 0,
                # A campaign measures any fastbrowse_ref, and builds before 0.11.2 have no image count on a cost line.
                "images": getattr(line, "images", None) or 0,
                # Builds before 0.11.3 report no prompt-cache counts.
                "cache_read": getattr(line, "cache_read_tokens", None) or 0,
                "cache_write": getattr(line, "cache_write_tokens", None) or 0,
                "cost_usd": line.dollars,
                "source": "harness",
            }
        )
    # The models that served the run, once each in the order they first appeared.
    actual_models = list(dict.fromkeys(entry["model"] for entry in usage if entry["model"]))
    pages, seen_at = read_pages(events)
    steps = []
    for step in output.steps:
        item: dict[str, object] = {
            "action": step_action(step.operation.value, step.outcome.value, step.note),
            "url": step.url,
        }
        if step.target:
            item["value"] = step.target
        if step.facts:
            item["quote"] = step.facts[0].quote or step.facts[0].text
        if step.index in seen_at:
            item["page"] = seen_at[step.index]
        steps.append(item)
    return {
        "status": _turn_status(output.status),
        "answer": result_answer(output.answer, output.data),
        "error": output.error,
        "final_page": page,
        "final_url": output.final_url,
        "step_log": steps,
        "pages": pages,
        "citations": evidence_citations(output.citations),
        "cost_lines": lines,
        "usage": usage,
        "latency_ms": latency_ms,
        "actual_models": actual_models,
    }
