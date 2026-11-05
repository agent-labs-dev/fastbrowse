"""The fastbrowse-pool arm: lease, capacity waits, keep-alive and release, all against an offline mock pool."""

import asyncio
import json
import time
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from fastbrowse.models import CostBreakdown, RunResult, Status

from fastbrowse_evals import backend, live
from fastbrowse_evals.attempt_clock import TIMING_BASIS
from fastbrowse_evals.final_page import FinalPage
from fastbrowse_evals.live_tasks import TASKS, Outcome

POOL = "https://pool.example.test"
TOKEN = "pool-secret"


class MockPool:
    """Answers the browser pool's lease API; `full` 503s come first, and `internal` mimics TLS ended at the edge."""

    def __init__(self, *, full: int = 0, internal: bool = False, discovery: int = 200, prefix: str = "") -> None:
        self.full, self.internal, self.discovery, self.prefix = full, internal, discovery, prefix
        self.requests: list[httpx.Request] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        assert request.headers["Authorization"] == f"Bearer {TOKEN}"
        base = "http://pool.example.test" if self.internal else POOL
        path = request.url.raw_path.decode().split("?")[0]  # the lease id stays percent-encoded
        if "/leases" in path:  # the lease API is served under the pool's path prefix, if it has one
            if not path.startswith(f"{self.prefix}/leases"):
                return httpx.Response(404)
            path = path.removeprefix(self.prefix)
        if request.method == "POST" and path == "/leases":
            if self.full:
                self.full -= 1
                return httpx.Response(503, json={"detail": "PoolExhausted"})
            body = json.loads(request.content)
            assert body == {
                "organization_id": "parallax-benchmark",
                "run_id": body["run_id"],
                "timeout_minutes": 120,
            }
            return httpx.Response(
                200,
                json={
                    "lease_id": "lease/1",
                    "cdp_url": f"{base}/browsers/b1?x=1",
                    "expires_at": "2026-10-16T12:00:00Z",
                    "profile_applied": False,
                },
            )
        if path == "/browsers/b1/json/version":
            ws = f"{'ws' if self.internal else 'wss'}://pool.example.test/devtools/browser/b1?t=abc"
            return httpx.Response(self.discovery, json={"webSocketDebuggerUrl": ws})
        if request.method == "POST" and path == "/leases/lease%2F1/extend":
            assert request.url.params["seconds"] == "900"
            return httpx.Response(200, json={})
        if request.method == "DELETE" and path == "/leases/lease%2F1":
            return httpx.Response(204)
        return httpx.Response(404)

    def calls(self, method: str, suffix: str) -> int:
        return sum(r.method == method and r.url.raw_path.decode().endswith(suffix) for r in self.requests)


@pytest.fixture
def pool_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PARALLAX_POOL_URL", POOL)
    monkeypatch.setenv("PARALLAX_POOL_TOKEN", TOKEN)


async def test_lease_acquires_extends_and_releases(pool_env: None, monkeypatch: pytest.MonkeyPatch) -> None:
    pool = MockPool()
    monkeypatch.setattr(live, "POOL_EXTEND_EVERY", 0.01)
    async with httpx.AsyncClient(transport=httpx.MockTransport(pool)) as http:
        async with live.pool_lease(http, "run-1") as lease:
            assert lease.ws_url == "wss://pool.example.test/devtools/browser/b1?t=abc"
            for _ in range(200):
                if pool.calls("POST", "/extend?seconds=900") >= 2:
                    break
                await asyncio.sleep(0.01)
            assert pool.calls("DELETE", "/leases/lease%2F1") == 0
        extended = pool.calls("POST", "/extend?seconds=900")
        await asyncio.sleep(0.05)
    assert extended >= 2 and pool.calls("POST", "/extend?seconds=900") == extended  # stops with the run
    assert pool.calls("DELETE", "/leases/lease%2F1") == 1
    assert lease.extend_failures == 0


async def test_full_pool_is_waited_out_not_failed(pool_env: None, monkeypatch: pytest.MonkeyPatch) -> None:
    waits: list[float] = []
    real_sleep = asyncio.sleep

    async def no_wait(seconds: float) -> None:
        waits.append(seconds)
        await real_sleep(0)

    pool = MockPool(full=6)
    monkeypatch.setattr(live.asyncio, "sleep", no_wait)
    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(pool)) as http,
        live.pool_lease(http, "run-2") as lease,
    ):
        assert lease.ws_url.startswith("wss://")
    assert waits[:6] == [2, 4, 8, 16, 30, 30]
    assert pool.calls("POST", "/leases") == 7 and pool.calls("DELETE", "/leases/lease%2F1") == 1


async def test_a_pool_behind_a_path_prefix_extends_and_releases_under_it(monkeypatch: pytest.MonkeyPatch) -> None:
    """A lease not managed under the prefix it was taken from is never extended or returned."""
    monkeypatch.setenv("PARALLAX_POOL_URL", f"{POOL}/base/")
    monkeypatch.setenv("PARALLAX_POOL_TOKEN", TOKEN)
    monkeypatch.setattr(live, "POOL_EXTEND_EVERY", 0.01)
    mock = MockPool(prefix="/base")
    async with httpx.AsyncClient(transport=httpx.MockTransport(mock)) as http, live.pool_lease(http, "run-4"):
        for _ in range(200):
            if mock.calls("POST", "/base/leases/lease%2F1/extend?seconds=900"):
                break
            await asyncio.sleep(0.01)
    assert mock.calls("POST", "/base/leases/lease%2F1/extend?seconds=900") >= 1
    assert mock.calls("DELETE", "/base/leases/lease%2F1") == 1


async def test_internal_addresses_move_onto_the_pool_origin(pool_env: None) -> None:
    pool = MockPool(internal=True)
    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(pool)) as http,
        live.pool_lease(http, "run-3") as lease,
    ):
        assert lease.ws_url == "wss://pool.example.test/devtools/browser/b1?t=abc"
    discovery = next(r for r in pool.requests if r.url.path.endswith("/json/version"))
    assert str(discovery.url) == "https://pool.example.test/browsers/b1/json/version?x=1"
    release = next(r for r in pool.requests if r.method == "DELETE")
    assert str(release.url) == "https://pool.example.test/leases/lease%2F1"
    assert live.on_pool_origin("http://10.0.0.5:9222/a/b?q=1", POOL) == "https://pool.example.test/a/b?q=1"
    assert live.on_pool_origin("ws://10.0.0.5:9222/d?t=x", "http://pool.local:8080") == "ws://pool.local:8080/d?t=x"


async def test_lease_is_released_when_discovery_or_the_run_fails(pool_env: None) -> None:
    pool = MockPool(discovery=502)
    async with httpx.AsyncClient(transport=httpx.MockTransport(pool)) as http:
        with pytest.raises(live.Unavailable, match="HTTP 502"):
            async with live.pool_lease(http, "run-4"):
                pytest.fail("no browser to hand out")
        assert pool.calls("DELETE", "/leases/lease%2F1") == 1
        pool = MockPool()
        http._transport = httpx.MockTransport(pool)
        with pytest.raises(RuntimeError, match="agent crashed"):
            async with live.pool_lease(http, "run-5"):
                raise RuntimeError("agent crashed")
    assert pool.calls("DELETE", "/leases/lease%2F1") == 1


async def test_pool_needs_both_settings_and_never_records_the_token(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("PARALLAX_POOL_URL", raising=False)
    monkeypatch.delenv("PARALLAX_POOL_TOKEN", raising=False)
    with pytest.raises(RuntimeError, match="PARALLAX_POOL_URL and PARALLAX_POOL_TOKEN"):
        live.pool_settings()
    monkeypatch.setenv("PARALLAX_POOL_URL", f"{POOL}/base")
    monkeypatch.setenv("PARALLAX_POOL_TOKEN", TOKEN)
    identity = live.browser_identity("fastbrowse-pool", None)
    assert identity == {"mode": "fastbrowse-pool", "host": "pool.example.test"}
    assert TOKEN not in json.dumps(identity)


async def test_pool_arm_runs_fastbrowse_unchanged_but_on_the_leased_browser(
    pool_env: None, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    pool = MockPool(full=1)
    seen: dict[str, Any] = {}
    wiki = next(t for t in TASKS if t.id == "wiki-open")
    wiki_url = "https://en.wikipedia.org/wiki/Kurt_G%C3%B6del"
    real_sleep = asyncio.sleep

    async def slow_capacity(seconds: float) -> None:
        await real_sleep(0.05)

    async def run_task(goal: str, **kwargs: Any) -> RunResult:
        seen.update(kwargs, goal=goal, cdp_url=await kwargs["open_browser"]())
        return RunResult(
            status=Status.COMPLETE, answer="done", data=None, evidence=(), steps=(), cost=CostBreakdown(), artifacts=()
        )

    @asynccontextmanager
    async def held(cdp_url: str, *, viewport: bool) -> AsyncIterator[None]:
        seen["held"] = (cdp_url, viewport)
        yield

    monkeypatch.setattr(live, "run_task", run_task)
    monkeypatch.setattr(backend, "environment", lambda: {"OPENROUTER_API_KEY": "or"})
    monkeypatch.setattr(live, "cloud_browser", Mock(side_effect=AssertionError("the pool arm starts no cloud browser")))
    monkeypatch.setattr(live, "prepared_tab", held)
    monkeypatch.setattr(live, "observe_browser", AsyncMock(return_value=FinalPage(url=wiki_url)))
    monkeypatch.setattr(live.asyncio, "sleep", slow_capacity)
    async with httpx.AsyncClient(transport=httpx.MockTransport(pool)) as http:
        began = time.monotonic()
        outcome, report = await live.ARMS["fastbrowse-pool"].runner(wiki, http, tmp_path, record=None)
        elapsed = time.monotonic() - began
    assert seen["cdp_url"] == "wss://pool.example.test/devtools/browser/b1?t=abc"
    assert "browser_api_key" not in seen and "chrome" not in seen
    # fastbrowse attaches to the tab the harness prepared, held at the competing arm's viewport, so the backend
    # comparison runs the same agent the same way on both browsers.
    assert seen["held"] == (seen["cdp_url"], True)
    assert outcome.final_url == wiki_url
    assert seen["limits"] == live.AGENT_LIMITS and seen["start"] == wiki.start
    assert isinstance(outcome, Outcome) and report.status == "complete"
    # Getting the browser is opening its session, outside the run's seconds as a cloud browser's start is.
    assert report.pool_full_seconds is not None and report.pool_full_seconds >= 0.05
    assert report.lease_seconds is not None and report.session_open_seconds == report.lease_seconds
    assert report.session_close_seconds is not None and report.timing_basis == TIMING_BASIS
    # Holds however slow the machine: lease and full-pool waits sit inside the call but outside the run's own seconds.
    assert report.seconds + report.lease_seconds + report.pool_full_seconds <= elapsed + 0.02
    assert report.lease_extend_failures == 0
    assert pool.calls("DELETE", "/leases/lease%2F1") == 1


def test_pool_arm_covers_exactly_the_fastbrowse_tasks() -> None:
    tasks = [t for suite in live.SUITES.values() for t in suite]
    pool = {t.id for t in tasks if live.eligible("fastbrowse-pool", t)}
    assert pool == {t.id for t in tasks if live.eligible("fastbrowse", t)} and len(pool) == 53
    spec = live.ARMS["fastbrowse-pool"]
    assert (spec.tier, spec.default, spec.env_allowlist) == ("A", False, ())
    assert spec.pin == "run.fastbrowse_version + run.git_sha; fastbrowse-hosted browser pool"


@pytest.mark.parametrize("arm", ["fastbrowse", "fastbrowse-pool"])
async def test_a_safe_stop_on_either_backend_is_graded_as_fastbrowse(monkeypatch: pytest.MonkeyPatch, arm: str) -> None:
    """The pool arm is Fastbrowse on another browser; the judge sees its safe stop exactly as Fastbrowse's (never the
    arm's name), so a correct safe stop the judge accepts passes on both backends."""
    outcome = Outcome(None, None, "https://www.saucedemo.com/checkout-step-two.html")
    report = live.ArmReport(status="needs_confirmation", seconds=3, dollars=0.01)
    judged: list[dict[str, Any]] = []
    routed: list[str | None] = []

    async def runner(*_: Any, **__: Any) -> tuple[Outcome, live.ArmReport]:
        return outcome, report

    async def judge(
        state: dict[str, Any],
        *,
        policy: str,
        questions: dict[str, str] | None = None,
        fallback_only: str | None,
        unjudgeable: str | None = None,
        use_jev: bool = True,
    ) -> tuple[Any, str]:
        judged.append(state)
        routed.append(fallback_only if use_jev else "pinned judge alone")
        return SimpleNamespace(policy=policy), "policy"

    available = {"passed": True, "source": "llm", "reason": "observed"}
    judgment = {"policy": "policy", "state_sha256": "e" * 64, "questions": {}}
    judgment["decisions"] = {"evidence_available": available}
    receipt = {"judgment": judgment, "judge_cost_usd": 0.0, "known_judge_cost_usd": 0.0, "unpriced_judge_attempts": 0}
    monkeypatch.setitem(live.ARMS, arm, live.ARMS[arm].model_copy(update={"runner": runner}))
    monkeypatch.setattr(live, "judge_browser", judge)
    monkeypatch.setattr(live, "browser_verdict", lambda _: (True, "safe stop before the irreversible order"))
    monkeypatch.setattr(live, "judge_receipt", lambda *_: receipt)
    task = next(t for t in TASKS if t.id == "saucedemo-pause")
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        row = await live.run_arm(arm, task, None, http, Path(), record=None)
    [state] = judged
    assert arm not in json.dumps(state)
    assert state["expected_ending"] == "stopped" and state["report"]["ending"] == "stopped"
    assert row.passed and row.correct and row.failure is None
    assert routed[0] is not None  # a safety stop is judged by the pinned fallback on either backend
