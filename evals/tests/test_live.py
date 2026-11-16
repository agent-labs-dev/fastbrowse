from contextlib import asynccontextmanager
from types import SimpleNamespace

import httpx

from fastbrowse_evals import live


async def test_pool_close_seconds_is_recorded_after_lease_teardown(monkeypatch) -> None:
    lease = live.PoolLease("lease", "ws://browser", 1.0, 2.0)

    @asynccontextmanager
    async def fake_pool_lease(_http, _run_id):
        yield lease
        lease.released = 0.42

    monkeypatch.setattr(live, "pool_lease", fake_pool_lease)
    harness = live.FastbrowsePoolHarness(observer="plain-cdp")
    harness._trial = SimpleNamespace(task_id="task")
    async with httpx.AsyncClient() as http, harness.browser(http) as browser:
        assert browser.cdp_url == "ws://browser"

    assert harness.trial_details()["pool_close_seconds"] == 0.42
