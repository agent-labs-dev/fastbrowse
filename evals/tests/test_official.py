"""The official-suite runner: bounded judge billing, durable resume, private output and an allowlisted child process."""

import argparse
import asyncio
import base64
import contextlib
import hashlib
import importlib.util
import json
import os
import stat
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import httpx
import pytest
from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine, Limits, RunResult

from fastbrowse_evals import backend, judging, official
from fastbrowse_evals.attempt_clock import SESSION_INCLUDED, TIMING_BASIS
from fastbrowse_evals.backend import NotStarted
from fastbrowse_evals.browsers import AGENT_SCREEN, Browser
from fastbrowse_evals.registry import Suite

_spec = importlib.util.spec_from_file_location("official_bridge", official.BRIDGE)
assert _spec is not None and _spec.loader is not None
bridge_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bridge_module)


_OFFLINE: dict[str, str] = {}


def settings(**values: str) -> None:
    """The environment every later read sees, and nothing else: a real key in the shell never reaches a test, and a
    failing assertion cannot print one."""
    _OFFLINE.clear()
    _OFFLINE.update(values)


@pytest.fixture(autouse=True)
def offline_settings(monkeypatch):
    """Keys come from `settings(...)` alone, so no test reads the environment or a .env file for keys."""
    _OFFLINE.clear()
    monkeypatch.setattr(backend, "environment", lambda: dict(_OFFLINE))


BILLING = {
    "input_usd_per_mtok": 1.0,
    "output_usd_per_mtok": 2.0,
    "max_requests": 2,
    "max_input_tokens": 100_000,
    "max_output_tokens": 1000,
    "image_tokens_per_image": 3000,
    "budget_usd": 0.01,
}


def chat(
    max_tokens: int | None = 512, *, url: str = "https://api.openai.com/v1/chat/completions", **extra: Any
) -> httpx.Request:
    body: dict[str, Any] = {"model": "m", "messages": [{"role": "user", "content": "x" * 100}], **extra}
    if max_tokens is not None:
        body["max_tokens"] = max_tokens
    return httpx.Request("POST", url, json=body)


def reply(request: httpx.Request, usage: dict[str, int] | None = None, status: int = 200) -> httpx.Response:
    return httpx.Response(status, json={"usage": usage} if usage else {"error": "boom"}, request=request)


def rows(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines()]


def mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


def meter(tmp_path: Path, **override: float) -> Any:
    return bridge_module.Meter({**BILLING, **override}, tmp_path / "receipts.jsonl")


# --- the meter: ceilings hold before a request leaves, and receipts survive failure ---


def test_receipt_is_durable_before_the_request_proceeds(tmp_path):
    m = meter(tmp_path)
    request = chat()
    m.before(request)
    reserved = rows(tmp_path / "receipts.jsonl")
    assert [r["event"] for r in reserved] == ["reserved"]
    assert reserved[0]["max_output"] == 512
    assert mode(tmp_path / "receipts.jsonl") == 0o600


def test_settled_usage_replaces_the_reservation(tmp_path):
    m = meter(tmp_path)
    request = chat()
    m.before(request)
    m.after(reply(request, {"prompt_tokens": 1000, "completion_tokens": 100}))
    settled = rows(tmp_path / "receipts.jsonl")[-1]
    assert settled["event"] == "settled"
    assert settled["usd"] == pytest.approx((1000 * 1.0 + 100 * 2.0) / 1e6)
    assert m.committed_usd == pytest.approx(settled["usd"])


def test_settled_receipt_names_the_model_the_endpoint_served(tmp_path):
    m = meter(tmp_path)
    request = chat()
    m.before(request)
    body = {
        "id": "gen-1",
        "model": "deepseek/deepseek-v4.1-flash",
        "usage": {"prompt_tokens": 10, "completion_tokens": 1},
    }
    m.after(httpx.Response(200, json=body, request=request))
    settled = rows(tmp_path / "receipts.jsonl")[-1]
    assert (settled["model"], settled["response_id"]) == ("deepseek/deepseek-v4.1-flash", "gen-1")


@pytest.mark.parametrize("tokens,expected", [(999, 0.001199), (1000, 0.0012), (1001, 0.002302)])
def test_context_tier_prices_all_input_and_output_only_above_threshold(tmp_path, tokens, expected):
    m = meter(tmp_path, context_threshold_tokens=1000, context_input_usd_per_mtok=2, context_output_usd_per_mtok=3)
    request = chat(messages=[{"role": "user", "content": "x" * 1100}])
    m.before(request)
    reserved = rows(tmp_path / "receipts.jsonl")[0]
    assert reserved["reserved_usd"] == pytest.approx((reserved["input_bound"] * 2 + 512 * 3) / 1e6)
    m.after(reply(request, {"prompt_tokens": tokens, "completion_tokens": 100}))
    assert m.committed_usd == pytest.approx(expected)
    resumed = meter(
        tmp_path, context_threshold_tokens=1000, context_input_usd_per_mtok=2, context_output_usd_per_mtok=3
    )
    assert resumed.committed_usd == pytest.approx(expected)


def test_context_tier_refuses_before_request_and_retains_unknown_charge(tmp_path):
    tier = {"context_threshold_tokens": 100, "context_input_usd_per_mtok": 100, "context_output_usd_per_mtok": 200}
    m = meter(tmp_path, **tier)
    with pytest.raises(bridge_module.BillingError, match="dollar ceiling"):
        m.before(chat())
    assert not any(r["event"] == "reserved" for r in rows(tmp_path / "receipts.jsonl"))
    unknown = meter(tmp_path / "unknown", budget_usd=1, **tier)
    request = chat()
    unknown.before(request)
    reservation = unknown.committed_usd
    unknown.after(reply(request, status=500))
    assert unknown.committed_usd == reservation


@pytest.mark.parametrize(
    "tier",
    [
        {"context_threshold_tokens": 100},
        {"context_threshold_tokens": 0, "context_input_usd_per_mtok": 2, "context_output_usd_per_mtok": 3},
        {"context_threshold_tokens": 1.5, "context_input_usd_per_mtok": 2, "context_output_usd_per_mtok": 3},
        {"context_threshold_tokens": 100.0, "context_input_usd_per_mtok": 2, "context_output_usd_per_mtok": 3},
        {"context_threshold_tokens": True, "context_input_usd_per_mtok": 2, "context_output_usd_per_mtok": 3},
        {"context_threshold_tokens": 100, "context_input_usd_per_mtok": 0.5, "context_output_usd_per_mtok": 3},
    ],
)
def test_invalid_context_pricing_fails_before_request(tmp_path, tier):
    with pytest.raises(bridge_module.BillingError):
        meter(tmp_path, **tier)
    args = SimpleNamespace(**{flag: BILLING[key] for flag, key in official.BILLING_FLAGS.items()})
    for flag, key in official.CONTEXT_PRICING_FLAGS.items():
        setattr(args, flag, tier.get(key))
    with pytest.raises(ValueError):
        official.billing_config(args)


def test_context_prices_are_recorded_and_changed_prices_refuse_resume(tmp_path, stubbed):
    argv = main_args(
        tmp_path,
        "--judge-context-threshold-tokens",
        "272000",
        "--judge-context-input-usd-per-mtok",
        "2",
        "--judge-context-output-usd-per-mtok",
        "3",
    )
    assert asyncio.run(official.main(argv)) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["judge_billing"]["context_threshold_tokens"] == 272000
    assert isinstance(manifest["judge_billing"]["context_threshold_tokens"], int)
    assert asyncio.run(official.main([*argv, "--resume"])) == 0
    changed = [*argv, "--resume", "--judge-context-output-usd-per-mtok", "4"]
    with pytest.raises(SystemExit):
        asyncio.run(official.main(changed))
    assert stubbed == []


def test_an_errored_request_stays_charged_at_its_worst_case(tmp_path):
    m = meter(tmp_path)
    request = chat()
    m.before(request)
    reserved = m.committed_usd
    m.after(reply(request, None, status=500))
    assert m.committed_usd == pytest.approx(reserved)
    assert rows(tmp_path / "receipts.jsonl")[-1]["output_tokens"] is None


def test_a_request_that_never_answers_is_still_charged_and_logged(tmp_path):
    m = meter(tmp_path)
    m.before(chat())
    summary = m.close("ConnectError")
    assert summary["unsettled_requests"] == 1
    assert summary["committed_usd"] > 0
    assert rows(tmp_path / "receipts.jsonl")[-1]["error_type"] == "ConnectError"


@pytest.mark.parametrize(
    ("override", "request_kwargs", "reason"),
    [
        ({"max_requests": 1}, {}, "request ceiling"),
        ({"max_input_tokens": 50}, {}, "input token ceiling"),
        ({"budget_usd": 0.0001}, {}, "dollar ceiling"),
        ({}, {"max_tokens": None}, "output token ceiling"),
        ({}, {"max_tokens": 5000}, "output ceiling exceeds"),
        ({}, {"url": "https://example.com/v1/chat/completions"}, "pinned judge endpoint"),
        ({}, {"url": "https://api.openai.com/v1/embeddings"}, "pinned judge endpoint"),
    ],
)
def test_ceilings_refuse_before_any_request_is_reserved(tmp_path, override, request_kwargs, reason):
    m = meter(tmp_path, **override)
    if "request ceiling" in reason:
        m.before(chat())
    spent = m.committed_usd
    with pytest.raises(bridge_module.BillingError, match=reason):
        m.before(chat(**request_kwargs))
    assert m.committed_usd == spent
    assert rows(tmp_path / "receipts.jsonl")[-1]["event"] == "refused"


def test_once_refused_every_later_request_is_refused(tmp_path):
    m = meter(tmp_path, max_requests=1)
    m.before(chat())
    with pytest.raises(bridge_module.BillingError):
        m.before(chat())
    with pytest.raises(bridge_module.BillingError, match="request ceiling"):
        m.before(chat())


def test_images_are_reserved_at_the_operators_verified_ceiling(tmp_path):
    m = meter(tmp_path, max_input_tokens=2000)
    with pytest.raises(bridge_module.BillingError, match="input token"):
        m.before(chat(messages_extra=[{"type": "image_url", "image_url": {"url": "data:,"}}]))


def test_billing_without_an_image_ceiling_fails_closed(tmp_path):
    without = {k: v for k, v in BILLING.items() if k != "image_tokens_per_image"}
    with pytest.raises(bridge_module.BillingError):
        bridge_module.Meter(without, tmp_path / "r.jsonl")


def test_billing_needs_every_explicit_limit(tmp_path):
    incomplete = {k: v for k, v in BILLING.items() if k != "budget_usd"}
    with pytest.raises(bridge_module.BillingError):
        bridge_module.Meter(incomplete, tmp_path / "r.jsonl")
    with pytest.raises(bridge_module.BillingError):
        bridge_module.Meter({**BILLING, "output_usd_per_mtok": 0}, tmp_path / "r.jsonl")


def test_a_second_process_inherits_the_first_process_commitments(tmp_path):
    first = meter(tmp_path)
    first.before(chat())
    second = meter(tmp_path)
    assert second.requests == 1
    assert second.committed_usd == pytest.approx(first.committed_usd)
    second.before(chat())
    with pytest.raises(bridge_module.BillingError, match="request ceiling"):
        meter(tmp_path).before(chat())


def test_parent_receipt_total_matches_the_meter(tmp_path):
    m = meter(tmp_path, max_requests=5)
    first, second = chat(), chat()
    m.before(first)
    m.after(reply(first, {"prompt_tokens": 10, "completion_tokens": 5}))
    m.before(second)
    assert official.receipts_usd(tmp_path / "receipts.jsonl") == pytest.approx(m.committed_usd)
    assert official.receipts_usd(tmp_path / "missing.jsonl") == 0


def test_transport_refuses_before_the_network_and_records_usage(tmp_path):
    m = meter(tmp_path, max_requests=1)
    sent: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        sent.append(request)
        return httpx.Response(200, json={"usage": {"prompt_tokens": 7, "completion_tokens": 3}})

    client = httpx.Client(
        transport=httpx.MockTransport(handler), event_hooks={"request": [m.before], "response": [m.sync_after]}
    )
    url = "https://api.openai.com/v1/chat/completions"
    client.post(url, json={"max_tokens": 10, "messages": []})
    with pytest.raises(bridge_module.BillingError):
        client.post(url, json={"max_tokens": 10, "messages": []})
    assert len(sent) == 1
    assert [r["event"] for r in rows(tmp_path / "receipts.jsonl")] == ["reserved", "settled", "refused"]


def test_async_transport_meters_every_attempt(tmp_path):
    m = meter(tmp_path, max_requests=2)
    sent: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        sent.append(1)
        status = 429 if len(sent) == 1 else 200
        return httpx.Response(status, json={"usage": {"prompt_tokens": 1, "completion_tokens": 1}})

    async def run() -> None:
        client = httpx.AsyncClient(
            transport=httpx.MockTransport(handler), event_hooks={"request": [m.abefore], "response": [m.aafter]}
        )
        url = "https://api.openai.com/v1/chat/completions"
        body = {"max_completion_tokens": 10, "messages": []}
        await client.post(url, json=body)  # a retried attempt is a separate request
        await client.post(url, json=body)
        with pytest.raises(bridge_module.BillingError):
            await client.post(url, json=body)

    asyncio.run(run())
    assert len(sent) == 2
    assert m.requests == 2


def test_judge_client_that_bypasses_the_meter_fails_closed(tmp_path, monkeypatch):
    judge = SimpleNamespace(http_client=None, get_client=lambda: SimpleNamespace(_client=object()))
    fake = SimpleNamespace(
        load_tasks=lambda _: [{"task_id": "t"}], create_judge=lambda *a, **k: judge, judge_trace=None
    )
    monkeypatch.setitem(sys.modules, "evaluation", fake)
    with pytest.raises(bridge_module.SeamError):
        bridge_module.judge_bu(
            {"task_id": "t", "judge_model": "m", "reasoning": "x"},
            meter(tmp_path),
        )


def test_probe_fails_when_the_call_seam_moves():
    class Moved:
        def ainvoke(self):
            return self.other_client().create()

    with pytest.raises(bridge_module.SeamError):
        bridge_module.require_marker(Moved.ainvoke, "self.get_client().chat.completions.create")


# --- environment, permissions and the prepared interpreter ---


def test_child_environment_is_an_allowlist(tmp_path, monkeypatch):
    for name in ("OPENAI_API_KEY", "OPENAI_BASE_URL", "AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN", "PYTHONPATH"):
        monkeypatch.setenv(name, "leak")
    monkeypatch.setenv("PATH", "/usr/bin")
    env = official.child_env(tmp_path)
    assert "leak" not in env.values()
    assert set(env) <= {*official.ENV_ALLOWLIST, "HOME", "TMPDIR", "XDG_CONFIG_HOME", "BROWSER_USE_CONFIG_DIR",
                        "ANONYMIZED_TELEMETRY", "BROWSER_USE_SETUP_LOGGING", "PYTHONDONTWRITEBYTECODE",
                        "PYTHONNOUSERSITE"}  # fmt: skip
    assert env["HOME"] == str(tmp_path / "judge-home") and mode(tmp_path / "judge-home") == 0o700
    assert official.child_env(tmp_path, key="sk-test")["OPENAI_API_KEY"] == "sk-test"
    routed = official.child_env(tmp_path, key="sk-test", base_url="https://openrouter.ai/api/v1")
    assert routed["OPENAI_BASE_URL"] == "https://openrouter.ai/api/v1"


def test_meter_admits_only_the_recorded_judge_host(tmp_path):
    m = bridge_module.Meter(BILLING, tmp_path / "receipts.jsonl", host="openrouter.ai")
    m.before(chat(url="https://openrouter.ai/api/v1/chat/completions"))
    with pytest.raises(bridge_module.BillingError, match="pinned judge endpoint"):
        m.before(chat())


def test_private_files_and_rows(tmp_path):
    official.write_private(tmp_path / "a.png", b"x")
    official.append_row(tmp_path / "attempts.jsonl", {"task": "t"})
    assert mode(tmp_path / "a.png") == mode(tmp_path / "attempts.jsonl") == 0o600


def upstream_root(tmp_path: Path, script: str) -> argparse.Namespace:
    root = tmp_path / "upstream"
    python = root / ".venv" / "bin" / "python"
    python.parent.mkdir(parents=True)
    python.write_text(script)
    python.chmod(0o755)
    out = tmp_path / "out"
    out.mkdir()
    return argparse.Namespace(
        upstream_root=root, upstream_commit="a" * 40, judge_python=python, out=out, suite=Suite.MIND2WEB
    )


def test_bridge_runs_the_prepared_python_isolated_with_private_stderr(tmp_path, monkeypatch):
    dump = tmp_path / "seen.json"
    args = upstream_root(
        tmp_path,
        f"#!{sys.executable}\nimport json, os, sys\n"
        f"json.dump({{'argv': sys.argv[1:3], 'env': dict(os.environ), 'cwd': os.getcwd()}}, open({str(dump)!r}, 'w'))\n"
        "sys.stderr.write('secret diagnostic')\nprint(json.dumps({'seams_ok': True}))\n",
    )
    monkeypatch.setattr(official, "checkout", lambda *_: None)
    monkeypatch.setenv("OPENAI_API_KEY", "sk-judge")
    monkeypatch.setenv("AWS_SECRET_ACCESS_KEY", "leak")
    assert asyncio.run(official.bridge(args, {"suite": "online-mind2web", "operation": "judge"}, "judge-1")) == {
        "seams_ok": True
    }
    seen = json.loads(dump.read_text())
    assert seen["argv"] == ["-I", "-B"]
    assert seen["env"]["OPENAI_API_KEY"] == "sk-judge" and "AWS_SECRET_ACCESS_KEY" not in seen["env"]
    assert Path(seen["cwd"]).resolve() == (args.out / "judge-home").resolve()
    stderr = args.out / "diagnostics" / "judge-1.stderr"
    assert stderr.read_text() == "secret diagnostic" and mode(stderr) == 0o600
    assert mode(args.out / "diagnostics") == 0o700


def test_load_and_probe_never_receive_the_provider_key(tmp_path, monkeypatch):
    dump = tmp_path / "seen.json"
    args = upstream_root(
        tmp_path,
        f"#!{sys.executable}\nimport json, os\njson.dump(dict(os.environ), open({str(dump)!r}, 'w'))\nprint('[]')\n",
    )
    monkeypatch.setattr(official, "checkout", lambda *_: None)
    monkeypatch.setenv("OPENAI_API_KEY", "sk-judge")
    asyncio.run(official.bridge(args, {"suite": "bu-bench", "operation": "load"}, "load"))
    assert "OPENAI_API_KEY" not in json.loads(dump.read_text())


def test_judge_python_must_be_the_prepared_environment_inside_the_checkout(tmp_path):
    args = upstream_root(tmp_path, "#!/bin/sh\n")
    assert official.judge_python(args) == args.judge_python
    args.judge_python = Path(sys.executable)
    with pytest.raises(ValueError, match="prepared virtualenv"):
        official.judge_python(args)


def test_a_bridge_failure_leaves_the_run_ungraded_without_echoing_stderr(tmp_path, monkeypatch):
    args = upstream_root(tmp_path, f"#!{sys.executable}\nimport sys\nsys.stderr.write('prompt text')\nsys.exit(1)\n")
    monkeypatch.setattr(official, "checkout", lambda *_: None)
    with pytest.raises(ValueError, match="ungraded") as caught:
        asyncio.run(official.bridge(args, {"suite": "bu-bench", "operation": "load"}, "load"))
    assert "prompt text" not in str(caught.value)


# --- tasks, start site, metadata ---


def mind2web_dataset(tmp_path: Path, count: int = 300, **row: Any) -> argparse.Namespace:
    rows = [
        {"task_id": f"t{i}", "confirmed_task": f"do {i}", "website": "https://example.com/start", "level": "hard",
         "reference_length": 7, **row}
        for i in range(count)
    ]  # fmt: skip
    path = tmp_path / "tasks.json"
    path.write_text(json.dumps(rows))
    return argparse.Namespace(
        suite=Suite.MIND2WEB, dataset=path, dataset_sha256=hashlib.sha256(path.read_bytes()).hexdigest()
    )


def test_mind2web_tasks_keep_their_official_metadata(tmp_path):
    tasks = asyncio.run(official.tasks_for(mind2web_dataset(tmp_path)))
    assert tasks[0]["level"] == "hard" and tasks[0]["reference_length"] == 7
    assert official.start_site(tasks[0]) == "https://example.com/start"


@pytest.mark.parametrize("website", [None, "", "javascript:alert(1)", "file:///etc/passwd", "example.com"])
def test_mind2web_task_without_a_usable_start_site_is_rejected(tmp_path, website):
    with pytest.raises(ValueError, match="start website"):
        asyncio.run(official.tasks_for(mind2web_dataset(tmp_path, website=website)))


# --- the per-task run: frames, timing, start site ---


class FakeStep:
    def __init__(self, index: int) -> None:
        self.index = index

    def model_dump(self, mode: str, exclude: set[str]) -> dict[str, Any]:
        return {"index": self.index, "operation": "click"}


class FakeEvent:
    def __init__(self, index: int, frame: bytes | None) -> None:
        self.step, self.frame = FakeStep(index), frame


def fake_run(final: bytes | None, seen: dict[str, Any]) -> Any:
    async def run_task(goal: str, *, start: str | None, on_event: Any, **_: Any) -> Any:
        seen["start"] = start
        seen["limits"] = _["limits"]
        await on_event(FakeEvent(0, b"before-0"))
        await on_event(FakeEvent(1, None))
        await on_event(FakeEvent(2, b"before-2"))
        result = SimpleNamespace(
            final_frame=final,
            answer="done",
            final_url="https://shop.example/done",
            error=None,
            steps=[0, 1, 2],
            status=SimpleNamespace(value="complete"),
            cost=SimpleNamespace(known_dollars=0.1, lines=[]),
            model_dump_json=lambda indent: "{}",
        )
        result.model_copy = lambda update: SimpleNamespace(**(vars(result) | update))
        return result

    return run_task


def run_args(tmp_path: Path, suite: Suite) -> argparse.Namespace:
    out = tmp_path / "run"
    out.mkdir()
    return argparse.Namespace(
        suite=suite, out=out, reserve_dollars=1.0, browser_reserve_dollars=0.0, authorize=False,
        judge_model="judge", judge_reasoning="xhigh", score_threshold=3, **{
            "judge_input_usd_per_mtok": 1.0, "judge_output_usd_per_mtok": 2.0, "judge_max_requests": 4,
            "judge_max_input_tokens": 1000, "judge_max_output_tokens": 2000, "judge_image_tokens": 3000, "judge_budget": 0.5},
    )  # fmt: skip


def patch_run(monkeypatch, final: bytes | None, grade: dict[str, Any]) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    seen: dict[str, Any] = {}
    requests: list[dict[str, Any]] = []

    async def fake_bridge(args: Any, request: dict[str, Any], name: str) -> dict[str, Any]:
        requests.append(request)
        return grade

    monkeypatch.setattr(official, "StepEvent", FakeEvent)
    monkeypatch.setattr(official, "run_task", fake_run(final, seen))
    monkeypatch.setattr(official, "bridge", fake_bridge)
    return seen, requests


def test_mind2web_run_starts_at_the_task_site_and_labels_mixed_frames(tmp_path, monkeypatch):
    seen, requests = patch_run(monkeypatch, b"after", {"score": 1.0, "passed": True})
    args = run_args(tmp_path, Suite.MIND2WEB)
    task = {"task_id": "t1", "confirmed_task": "do it", "website": "https://shop.example/", "level": "easy"}
    row = asyncio.run(official.execute_task(args, task, 0))
    assert seen["start"] == "https://shop.example/"
    judge = requests[-1]
    assert [Path(p).name for p in judge["screenshots"]] == ["step-0.png", "step-2.png", "final.png"]
    assert (args.out / "t1-0" / "final.png").read_bytes() == b"after"
    assert json.loads((args.out / "t1-0" / "task.json").read_text())["level"] == "easy"
    assert judge["billing"]["budget_usd"] == 0.5 and judge["receipts"].endswith("t1-0/receipts.jsonl")
    assert row["upstream_passed"] is True and row["agent_cost_unknown"] is False
    assert row["model"] == backend.providers() and "text_model" in row


@pytest.mark.parametrize(
    ("model", "reasoning"),
    [("openai/gpt-5.6-luna", True), ("o4-mini", True), ("openai/o4-mini", True), ("gpt-4o", False)],
)
def test_mind2web_reasoning_judges_get_the_reasoning_request(tmp_path, monkeypatch, model, reasoning):
    _, requests = patch_run(monkeypatch, b"after", {"score": 1.0, "passed": True})
    args = run_args(tmp_path, Suite.MIND2WEB)
    args.judge_model = model
    task = {"task_id": "t1", "confirmed_task": "do it", "website": "https://shop.example/"}
    asyncio.run(official.execute_task(args, task, 0))
    assert requests[-1]["reasoning_model"] is reasoning


def test_bu_run_gives_the_judge_the_step_frames_then_the_final_frame_in_order(tmp_path, monkeypatch):
    seen, requests = patch_run(monkeypatch, b"after", {"score": 0.5, "verdict": None})
    asyncio.run(official.execute_task(run_args(tmp_path, Suite.BU_BENCH), {"task_id": "b1", "confirmed_task": "go"}, 0))
    trace = requests[-1]["trace"]
    assert seen["start"] is None
    assert trace["screenshot_steps"] is None
    assert base64.b64decode(trace["screenshots_b64"][-1]) == b"after" and len(trace["screenshots_b64"]) > 1
    assert isinstance(trace["agent_steps"], list) and all(isinstance(s, str) for s in trace["agent_steps"])
    assert trace["screenshot_timing"] == "after"
    assert "before or after" in trace["evidence_notes"][0]
    assert requests[-1] and (tmp_path / "run" / "b1-0" / "step-0.png").exists()


def test_binary_downloads_are_retained_and_disclosed_to_the_judge(tmp_path, monkeypatch):
    _, requests = patch_run(monkeypatch, b"after", {"score": 0.5, "verdict": None})
    args = run_args(tmp_path, Suite.BU_BENCH)
    original = official.run_task

    async def with_downloads(*a: Any, **k: Any) -> Any:
        (k["downloads"]).mkdir(parents=True)
        (k["downloads"] / "a.txt").write_text("hello")
        (k["downloads"] / "b.bin").write_bytes(b"\xff\xfe")
        return await original(*a, **k)

    monkeypatch.setattr(official, "run_task", with_downloads)
    asyncio.run(official.execute_task(args, {"task_id": "b1", "confirmed_task": "go"}, 0))
    trace = requests[-1]["trace"]
    assert json.loads(trace["output_files_text"]) == {"a.txt": "hello"}
    assert "1 binary output" in " ".join(trace["evidence_notes"])
    retained = json.loads((args.out / "b1-0" / "binary-outputs.json").read_text())
    assert retained[0]["name"] == "b.bin" and (args.out / "b1-0" / "downloads" / "b.bin").exists()


def test_run_directory_inside_an_unignored_repo_is_rejected(tmp_path):
    subprocess.run(["git", "init", "-q", str(tmp_path / "repo")], check=True)
    with pytest.raises(ValueError, match="not ignored"):
        official.private_root(tmp_path / "repo" / "out")
    (tmp_path / "repo" / ".gitignore").write_text("out/\n")
    official.private_root(tmp_path / "repo" / "out")
    official.private_root(tmp_path / "elsewhere")


def test_duplicate_attempt_rows_are_rejected(tmp_path):
    for _ in range(2):
        official.append_row(tmp_path / "attempts.jsonl", {"task": "t", "repeat": 0})
    with pytest.raises(ValueError, match="two rows"):
        official.attempt_state(tmp_path)


@pytest.mark.parametrize("task_id", ["né", "a/b", "٣", ".."])
def test_task_ids_must_be_safe_ascii(tmp_path, task_id):
    assert official.SAFE_ID.fullmatch(task_id) is None


def test_bu_missing_final_frame_passes_only_available_evidence_to_upstream(tmp_path, monkeypatch):
    _, requests = patch_run(monkeypatch, None, {"score": 0.0, "verdict": False})
    row = asyncio.run(
        official.execute_task(run_args(tmp_path, Suite.BU_BENCH), {"task_id": "b1", "confirmed_task": "go"}, 0)
    )
    trace = requests[0]["trace"]
    assert trace["screenshot_steps"] is None
    assert trace["agent_steps"]
    assert "No final page capture was available" in " ".join(trace["evidence_notes"])
    assert row["upstream_score"] == 0.0
    assert row["upstream_passed"] is False
    assert row["final_capture_available"] is False
    assert not (tmp_path / "run" / "b1-0" / "final.png").exists()


@pytest.mark.parametrize("suite", [Suite.MIND2WEB, Suite.BU_BENCH])
def test_a_missing_final_frame_runs_the_attempt_again_then_grades_what_was_recorded(tmp_path, monkeypatch, suite):
    _, requests = patch_run(monkeypatch, None, {"score": 1.0, "passed": True, "verdict": True})
    args = run_args(tmp_path, suite)
    task = {"task_id": "b1", "confirmed_task": "go", "website": "https://example.com"}

    async def attempt(retries: int) -> dict:
        official.CAPTURE_RETRIES.set(retries)
        return await official.execute_task(args, task, 0)

    with pytest.raises(official.Interrupted) as interrupted:
        asyncio.run(attempt(1))
    assert interrupted.value.check["judge_cost_usd"] == 0.0
    assert requests == []
    (tmp_path / "run" / "b1-0").rename(tmp_path / "run" / "b1-0.interrupted-0")

    row = asyncio.run(attempt(0))
    assert len(requests) == 1
    assert row["final_capture_available"] is False
    assert row["upstream_score"] == 1.0


def test_out_of_range_upstream_score_is_not_accepted(tmp_path, monkeypatch):
    patch_run(monkeypatch, b"after", {"score": 7})
    with pytest.raises(ValueError, match="outside zero to one"):
        asyncio.run(
            official.execute_task(run_args(tmp_path, Suite.BU_BENCH), {"task_id": "b1", "confirmed_task": "go"}, 0)
        )


# --- durable resume and total budget ---


def main_args(tmp_path: Path, *extra: str) -> list[str]:
    root = tmp_path / "upstream"
    python = root / ".venv" / "bin" / "python"
    python.parent.mkdir(parents=True, exist_ok=True)
    python.write_text("#!/bin/sh\n")
    python.chmod(0o755)
    dataset = tmp_path / "tasks.json"
    return [
        "online-mind2web", "--upstream-root", str(root), "--upstream-commit", "a" * 40, "--judge-python", str(python),
        "--dataset", str(dataset), "--dataset-sha256", "0" * 64, "--judge-model", "judge", "--out", str(tmp_path / "out"),
        "--reserve-dollars", "0.5", "--browser-reserve-dollars", "0.5",
        "--judge-input-usd-per-mtok", "1", "--judge-output-usd-per-mtok", "2",
        "--judge-pricing-checked", "2026-10-07", "--judge-max-requests", "4", "--judge-max-input-tokens", "1000",
        "--judge-max-output-tokens", "2000", "--judge-image-tokens", "3000", "--judge-budget", "0.5", *extra,
    ]  # fmt: skip


@pytest.fixture
def stubbed(monkeypatch):
    calls: list[tuple[str, int]] = []
    tasks = [{"task_id": f"t{i}", "confirmed_task": "x", "website": "https://e.com"} for i in range(3)]

    async def fake_tasks(args: Any) -> list[dict[str, Any]]:
        return tasks

    async def fake_bridge(args: Any, request: dict[str, Any], name: str) -> dict[str, Any]:
        return {"seams_ok": True, "versions": {"openai": "1"}}

    async def fake_execute(args: Any, task: dict[str, Any], repeat: int) -> dict[str, Any]:
        calls.append((task["task_id"], repeat))
        (args.out / f"{task['task_id']}-{repeat}").mkdir()
        return {"task": task["task_id"], "repeat": repeat, "upstream_score": 1.0, "agent_dollars": 0.2,
                "agent_cost_unknown": False}  # fmt: skip

    monkeypatch.setattr(official, "checkout", lambda *_: None)
    monkeypatch.setattr(official, "tasks_for", fake_tasks)
    monkeypatch.setattr(official, "bridge", fake_bridge)
    monkeypatch.setattr(official, "execute_task", fake_execute)
    return calls


def test_preflight_makes_no_calls_and_writes_a_private_manifest(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    assert stubbed == []
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert "unspecified timing" in manifest["screenshot_timing"] and manifest["judge_billing"]["budget_usd"] == 0.5
    assert mode(tmp_path / "out" / "manifest.json") == 0o600 and mode(tmp_path / "out") == 0o700


def test_the_judge_route_is_recorded_and_must_be_https(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    assert json.loads((tmp_path / "out" / "manifest.json").read_text())["judge_base_url"] == OPENROUTER
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path / "plain", "--judge-base-url", "http://openrouter.ai/api/v1")))


def test_execute_requires_explicit_pricing_and_ceilings(tmp_path, stubbed):
    argv = main_args(tmp_path)
    argv = [*argv[: argv.index("--judge-budget")], "--execute", "--budget", "10"]
    with pytest.raises(SystemExit):
        asyncio.run(official.main(argv))
    assert stubbed == []


def test_unbounded_upstream_seam_blocks_execution(tmp_path, monkeypatch, stubbed):
    async def unsafe(*_: Any) -> dict[str, Any]:
        return {"seams_ok": False, "versions": {}}

    monkeypatch.setattr(official, "bridge", unsafe)
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10")))
    assert stubbed == []


def test_settled_spend_releases_reservations_for_remaining_tasks(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "1.9"))) == 0
    assert stubbed == [("t0", 0), ("t1", 0), ("t2", 0)]


def test_resume_never_repeats_a_started_attempt_and_counts_its_reservation(tmp_path, stubbed):
    out = tmp_path / "out"
    assert asyncio.run(official.main(main_args(tmp_path, "--budget", "3.3"))) == 0
    # t0 finished, t1 started and was interrupted (its agent may have spent money); t2 never started.
    official.append_row(out / "attempts.jsonl", {"task": "t0", "repeat": 0, "started": True, "reserved_usd": 1.5})
    official.append_row(
        out / "results.jsonl", {"task": "t0", "repeat": 0, "agent_dollars": 0.2, "agent_cost_unknown": False}
    )
    official.append_row(out / "attempts.jsonl", {"task": "t1", "repeat": 0, "started": True, "reserved_usd": 1.5})
    code = asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "3.3", "--resume")))
    assert stubbed == [("t2", 0)]
    assert code == 1  # the interrupted attempt keeps the run honest about being incomplete
    assert len(rows(out / "attempts.jsonl")) == 3 and len(rows(out / "results.jsonl")) == 2


def test_a_preflight_run_records_its_first_execution_budget(tmp_path, stubbed):
    """Publication needs the approved budget that gated every start, so the first execution fixes it."""
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    assert json.loads((tmp_path / "out" / "manifest.json").read_text())["total_budget"] is None
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10", "--resume"))) == 0
    assert json.loads((tmp_path / "out" / "manifest.json").read_text())["total_budget"] == 10


def test_resume_cannot_change_the_approved_budget(tmp_path, stubbed):
    out = tmp_path / "out"
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "1.6"))) == 1
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "100", "--resume")))
    assert stubbed == [("t0", 0)] and json.loads((out / "manifest.json").read_text())["total_budget"] == 1.6


def test_a_started_run_without_a_recorded_budget_is_refused(tmp_path, stubbed):
    out = tmp_path / "out"
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    official.append_row(out / "attempts.jsonl", {"task": "t0", "repeat": 0, "started": True, "reserved_usd": 1.5})
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10", "--resume")))
    assert stubbed == []


def test_total_budget_stops_before_overcommitting(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "1.6"))) == 1
    assert stubbed == [("t0", 0)]


def test_resume_retains_ungraded_results_and_releases_the_run_lock(tmp_path, stubbed):
    argv = main_args(tmp_path, "--execute", "--budget", "10")
    assert asyncio.run(official.main(argv)) == 0
    path = tmp_path / "out" / "results.jsonl"
    results = rows(path)
    results[0]["upstream_score"] = None
    path.write_text("".join(json.dumps(result) + "\n" for result in results))
    assert asyncio.run(official.main([*argv, "--resume"])) == 1
    assert stubbed == [("t0", 0), ("t1", 0), ("t2", 0)]


def test_resume_rejects_a_changed_configuration(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    with pytest.raises(SystemExit):
        asyncio.run(official.main([*main_args(tmp_path, "--resume")[:-1], "--judge-model", "other", "--resume"]))


def test_settled_attempt_cost_uses_actual_agent_and_judge_receipts(tmp_path):
    folder = tmp_path / "t0-0"
    folder.mkdir()
    official.append_row(folder / "receipts.jsonl", {"event": "reserved", "n": 1, "reserved_usd": 0.3})
    official.append_row(folder / "receipts.jsonl", {"event": "settled", "n": 1, "usd": 0.1})
    official.append_row(folder / "receipts.jsonl", {"event": "reserved", "n": 2, "reserved_usd": 0.2})
    result = {"agent_dollars": 0.4, "agent_cost_unknown": False}
    assert official.attempt_usd(folder, result, 1.0, 1.5) == pytest.approx(0.4 + 0.1 + 0.2)
    assert official.attempt_usd(folder, {**result, "agent_cost_unknown": True}, 1.0, 1.5) == pytest.approx(1.3)
    assert official.attempt_usd(folder, None, 1.0, 1.5) == 1.5
    assert os.path.exists(folder / "receipts.jsonl")


def test_resume_cannot_change_real_site_authorization(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["authorize_irreversible_actions"] is False
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--resume", "--authorize")))


def test_a_second_process_cannot_run_the_same_output_directory(tmp_path, stubbed):
    import fcntl

    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    held = os.open(tmp_path / "out" / "run.lock", os.O_WRONLY | os.O_CREAT)
    fcntl.flock(held, fcntl.LOCK_EX)
    try:
        with pytest.raises(SystemExit):
            asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10", "--resume")))
    finally:
        os.close(held)
    assert stubbed == []


def test_reasoning_judge_keeps_messages_and_declares_metered_completion_ceiling():
    sent = []
    choice = SimpleNamespace(finish_reason="stop", message=SimpleNamespace(content='Status: "success"'))

    def create(**kwargs):
        sent.append(kwargs)
        return SimpleNamespace(choices=[choice])

    model = SimpleNamespace(client=SimpleNamespace(chat=SimpleNamespace(completions=SimpleNamespace(create=create))))
    request = {"judge_model": "o4-mini", "reasoning": "high", "billing": {"max_output_tokens": 8192}}
    messages = [{"role": "user", "content": "official prompt"}]
    assert bridge_module.reasoning_generate(model, request)(messages) == ['Status: "success"']
    assert sent == [
        {"model": "o4-mini", "messages": messages, "max_completion_tokens": 8192, "reasoning_effort": "high"}
    ]


@pytest.mark.parametrize("finish,content", [("length", "Status: success"), ("stop", None), ("content_filter", "")])
def test_reasoning_judge_refuses_incomplete_text_before_upstream_failure_parser(finish, content):
    choice = SimpleNamespace(finish_reason=finish, message=SimpleNamespace(content=content))

    def create(**kwargs):
        return SimpleNamespace(choices=[choice])

    model = SimpleNamespace(client=SimpleNamespace(chat=SimpleNamespace(completions=SimpleNamespace(create=create))))
    request = {"judge_model": "o4-mini", "reasoning": "high", "billing": {"max_output_tokens": 8192}}
    with pytest.raises(ValueError, match="no complete verdict"):
        bridge_module.reasoning_generate(model, request)([])


def test_image_billing_uses_one_verified_ceiling_and_retains_text_bytes(tmp_path):
    m = meter(tmp_path)
    request = chat(
        messages=[
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "image_url in ordinary prose"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64," + "A" * 1_000_000}},
                ],
            }
        ]
    )
    m.before(request)
    bound = rows(tmp_path / "receipts.jsonl")[0]["input_bound"]
    assert 3000 < bound < 4000
    assert len(request.content) > 1_000_000


def test_cli_billing_preserves_integer_ceiling_on_the_wire(tmp_path):
    args = SimpleNamespace(**{flag: BILLING[key] for flag, key in official.BILLING_FLAGS.items()})
    billing = official.billing_config(args)
    assert billing is not None
    m = bridge_module.Meter(billing, tmp_path / "receipts.jsonl")

    def create(**kwargs):
        m.before(chat(max_tokens=None, **kwargs))
        return SimpleNamespace(
            choices=[SimpleNamespace(finish_reason="stop", message=SimpleNamespace(content="success"))]
        )

    model = SimpleNamespace(client=SimpleNamespace(chat=SimpleNamespace(completions=SimpleNamespace(create=create))))
    request = {"judge_model": "o4-mini", "reasoning": "medium", "billing": billing}
    assert bridge_module.reasoning_generate(model, request)([]) == ["success"]


def test_reasoning_judge_default_is_supported(tmp_path):
    args = official.build_parser().parse_args(main_args(tmp_path, "--judge-model", "o4-mini"))
    assert args.judge_reasoning == "medium"


def test_unsupported_reasoning_is_rejected_before_checkout(tmp_path, monkeypatch):
    def unexpected(*args):
        raise AssertionError("checkout must not precede configuration validation")

    monkeypatch.setattr(official, "checkout", unexpected)
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--judge-model", "o4-mini", "--judge-reasoning", "xhigh")))


# --- concurrent workers, shared ledger and reconciliation ---


def concurrent_stub(monkeypatch, tmp_path, cost_unknown=False):
    """Execution that blocks until released, recording how many attempts overlap."""
    state: dict[str, Any] = {"calls": [], "running": 0, "peak": 0, "release": asyncio.Event(), "hold": True}

    async def fake_tasks(args: Any) -> list[dict[str, Any]]:
        return [{"task_id": f"t{i}", "confirmed_task": "x", "website": "https://e.com"} for i in range(3)]

    async def fake_bridge(args: Any, request: dict[str, Any], name: str) -> dict[str, Any]:
        return {"seams_ok": True, "versions": {"openai": "1"}}

    async def fake_execute(args: Any, task: dict[str, Any], repeat: int) -> dict[str, Any]:
        state["calls"].append((task["task_id"], repeat))
        state["running"] += 1
        state["peak"] = max(state["peak"], state["running"])
        (args.out / f"{task['task_id']}-{repeat}").mkdir()
        try:
            if state["hold"]:
                await state["release"].wait()
        finally:
            state["running"] -= 1
        return {"upstream_score": 1.0, "agent_dollars": 0.2, "agent_cost_unknown": cost_unknown}

    monkeypatch.setattr(official, "checkout", lambda *_: None)
    monkeypatch.setattr(official, "tasks_for", fake_tasks)
    monkeypatch.setattr(official, "bridge", fake_bridge)
    monkeypatch.setattr(official, "execute_task", fake_execute)
    return state


async def settle(times: int = 20) -> None:
    for _ in range(times):
        await asyncio.sleep(0)


def test_concurrency_is_a_positive_integer_recorded_in_the_manifest_and_fingerprint(tmp_path, stubbed):
    assert official.build_parser().parse_args(main_args(tmp_path)).concurrency == 1
    for bad in ("0", "-2"):
        with pytest.raises(SystemExit):
            asyncio.run(official.main(main_args(tmp_path, "--concurrency", bad)))
    assert asyncio.run(official.main(main_args(tmp_path, "--concurrency", "3"))) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["concurrency"] == 3
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--concurrency", "2", "--resume")))


def test_concurrent_reservations_never_overcommit_and_tight_tasks_wait_for_reconciliation(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)

    async def scenario() -> int:
        run = asyncio.create_task(
            official.main(main_args(tmp_path, "--execute", "--budget", "3.3", "--concurrency", "3"))
        )
        await settle()
        # Two reserves of 1.5 fit in 3.3; the third must wait, not start and not stop the run.
        assert state["calls"] == [("t0", 0), ("t1", 0)] and not run.done()
        state["hold"] = False
        state["release"].set()
        return await run

    assert asyncio.run(scenario()) == 0
    assert state["peak"] == 2
    assert sorted(state["calls"]) == [("t0", 0), ("t1", 0), ("t2", 0)]
    attempts = rows(tmp_path / "out" / "attempts.jsonl")
    assert len(attempts) == len({(r["task"], r["repeat"]) for r in attempts}) == 3
    # The journal records the gate in the order it acted, so publication can replay the overlapping holds.
    from fastbrowse_evals import publication

    journal = rows(tmp_path / "out" / official.JOURNAL)
    assert {r["id"] for r in journal[:2]} == {"t0-0", "t1-0"} and all(r["committed"] == 1.5 for r in journal[:2])
    assert journal[2]["id"] in {"t0-0", "t1-0"} and journal[2]["committed"] != 1.5  # a settlement before t2's hold
    charges = {f"{r['task']}-{r['repeat']}": r["agent_dollars"] for r in rows(tmp_path / "out" / "results.jsonl")}
    publication.replay_start_gate(journal, charges, 3.3, 1.5, "online-mind2web")


def test_unknown_agent_cost_stays_reserved_and_the_idle_ledger_stops_the_run(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path, cost_unknown=True)
    state["hold"] = False
    # t0 settles at its whole agent budget (1.0), so 1.0 + 1.5 > 1.6 and nothing else may begin.
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "1.6", "--concurrency", "3"))) == 1
    assert state["calls"] == [("t0", 0)]


def test_interrupted_resume_skips_started_attempts_under_concurrency(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)
    state["hold"] = False
    out = tmp_path / "out"
    assert asyncio.run(official.main(main_args(tmp_path, "--budget", "10", "--concurrency", "2"))) == 0
    official.append_row(out / "attempts.jsonl", {"task": "t0", "repeat": 0, "started": True, "reserved_usd": 1.5})
    official.append_row(out / "attempts.jsonl", {"task": "t1", "repeat": 0, "started": True, "reserved_usd": 1.5})
    argv = main_args(tmp_path, "--execute", "--budget", "10", "--concurrency", "2", "--resume")
    assert asyncio.run(official.main(argv)) == 1
    assert state["calls"] == [("t2", 0)]
    assert len(rows(out / "attempts.jsonl")) == 3


def test_cancellation_keeps_started_receipts_and_resume_does_not_repeat_them(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)
    out = tmp_path / "out"

    async def scenario() -> None:
        run = asyncio.create_task(
            official.main(main_args(tmp_path, "--execute", "--budget", "10", "--concurrency", "2"))
        )
        await settle()
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run

    asyncio.run(scenario())
    assert state["running"] == 0  # every owned attempt unwound
    assert [r["task"] for r in rows(out / "attempts.jsonl")] == ["t0", "t1"]
    assert not (out / "results.jsonl").exists()
    state["hold"] = False
    argv = main_args(tmp_path, "--execute", "--budget", "10", "--concurrency", "2", "--resume")
    assert asyncio.run(official.main(argv)) == 1
    assert state["calls"] == [("t0", 0), ("t1", 0), ("t2", 0)]  # t0 and t1 were never repeated


# --- a failed judge keeps the agent evidence already saved ---


def saved_result(**cost: Any) -> RunResult:
    return RunResult.model_validate(
        {"status": "complete", "answer": "a", "data": None, "evidence": [], "steps": [], "artifacts": [],
         "cost": {"lines": cost.get("lines", [])}}
    )  # fmt: skip


def failing_judge_run(tmp_path, monkeypatch, result: Any) -> dict[str, Any]:
    async def run_task(goal: str, **_: Any) -> Any:
        return result

    async def broken_bridge(*_: Any) -> Any:
        raise ValueError("secret prompt text")

    monkeypatch.setattr(official, "run_task", run_task)
    monkeypatch.setattr(official, "bridge", broken_bridge)
    monkeypatch.setattr(official, "checkout", lambda *_: None)
    args = run_args(tmp_path, Suite.BU_BENCH)
    args.budget, args.repeat, args.concurrency = 10.0, 1, 1
    tasks = [({"task_id": "b1", "confirmed_task": "go"}, 0)]
    assert asyncio.run(official.run_attempts(args, tasks, 0.0, 1.5)) is True
    return rows(args.out / "results.jsonl")[0]


def test_failed_judge_reconciles_the_saved_validated_agent_result(tmp_path, monkeypatch):
    result = saved_result(lines=[{"component": "llm", "basis": "metered", "dollars": 0.25}])
    result = result.model_copy(update={"final_frame": b"png"})
    row = failing_judge_run(tmp_path, monkeypatch, result)
    assert row["upstream_score"] is None and row["error_type"] == "ValueError"
    assert row["status"] == "complete" and row["completed"] is True
    assert row["agent_dollars"] == pytest.approx(0.25) and row["agent_cost_unknown"] is False
    assert row["answer"] == "a" and row["final_url"] is None
    assert "secret prompt text" not in json.dumps(row)


def test_missing_or_malformed_agent_evidence_is_not_a_known_zero(tmp_path, monkeypatch):
    folder = tmp_path / "b1-0"
    folder.mkdir()
    assert official.saved_agent(folder) == {}
    (folder / "agent.json").write_text("{not json")
    assert official.saved_agent(folder) == {}
    (folder / "agent.json").write_text(json.dumps({"status": "complete"}))
    assert official.saved_agent(folder) == {}
    assert official.attempt_usd(folder, {"upstream_score": None}, 1.0, 1.5) == 1.0


# --- the recorded browser and provider configuration ---


def test_browser_plan_records_the_actual_browser_without_key_values(monkeypatch):
    settings(BROWSER_USE_API_KEY="sk-browser")
    browser, runtime = official.browser_plan()
    assert browser["browser_api_key"] == "sk-browser" and runtime["browser"] == {"mode": "browser-use-cloud"}
    assert browser["cloud_timeout_minutes"] == 240
    assert "sk-browser" not in json.dumps(runtime)
    settings(FASTBROWSE_HEADED="1")
    browser, runtime = official.browser_plan()
    returned_key = "browser_api_key" in browser  # never compare the value itself: a failing assertion would print it
    assert not returned_key and runtime["browser"]["mode"] == "local-chrome" and runtime["browser"]["headed"] is True


def test_manifest_fingerprints_the_browser_and_cost_note_follows_it(tmp_path, monkeypatch, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["agent_runtime"]["browser"]["mode"] == "local-chrome"
    settings(BROWSER_USE_API_KEY="sk-browser")
    with pytest.raises(SystemExit):  # the browser changed since the manifest was written
        asyncio.run(official.main(main_args(tmp_path, "--resume")))


@contextlib.asynccontextmanager
async def held_browser(_: httpx.AsyncClient, *, screen: tuple[int, int]):
    assert screen == AGENT_SCREEN
    held = Browser("wss://cloud/held", open_seconds=2.0)
    yield held
    held.close_seconds = 1.0
    held.cost = (CostLine(component=CostComponent.BROWSER, basis=CostBasis.METERED, dollars=0.07),)


def test_cost_note_names_the_browser_the_run_used(tmp_path, monkeypatch):
    patch_run(monkeypatch, b"after", {"score": 1.0, "passed": True})
    task = {"task_id": "b1", "confirmed_task": "g"}
    row = asyncio.run(official.execute_task(run_args(tmp_path, Suite.BU_BENCH), task, 0))
    assert row["cost_note"].startswith("Local Chrome")
    assert row["seconds"] >= 0
    timing_path = next(tmp_path.rglob("timing.json"))
    timing = json.loads(timing_path.read_text())
    assert timing["agent_seconds"] == row["seconds"]
    assert row["timing_basis"] == SESSION_INCLUDED
    settings(BROWSER_USE_API_KEY="sk-browser")
    monkeypatch.setattr(official, "cloud_browser", held_browser)
    (tmp_path / "again").mkdir()
    args = run_args(tmp_path / "again", Suite.BU_BENCH)
    row = asyncio.run(official.execute_task(args, task, 0))
    assert row["cost_note"].startswith("Browser Use Cloud browser")
    # The harness held the browser, so the run's time is its own, and opening and closing are kept apart.
    assert row["timing_basis"] == TIMING_BASIS
    assert (row["session_open_seconds"], row["session_close_seconds"]) == (2.0, 1.0)
    assert row["agent_dollars"] == pytest.approx(0.07)


@pytest.mark.parametrize(
    ("timing", "expected"),
    [
        (json.dumps({"agent_seconds": 12.5}), 12.5),
        (None, None),
        ("{broken", None),
        ("[]", None),
        (json.dumps({}), None),
        (json.dumps({"agent_seconds": "12"}), None),
        (json.dumps({"agent_seconds": True}), None),
        (json.dumps({"agent_seconds": -1}), None),
        ('{"agent_seconds": NaN}', None),
    ],
)
def test_bad_timing_never_discards_the_known_agent_cost(tmp_path, timing, expected):
    result = saved_result(lines=[{"component": "llm", "basis": "metered", "dollars": 0.25}])
    (tmp_path / "agent.json").write_text(result.model_dump_json())
    if timing is not None:
        (tmp_path / "timing.json").write_text(timing)
    saved = official.saved_agent(tmp_path)
    assert saved["agent_dollars"] == pytest.approx(0.25) and saved["agent_cost_unknown"] is False
    assert saved.get("seconds") == expected


@pytest.mark.parametrize(
    "flags",
    [
        ["--max-steps", "100"],
        ["--task-timeout", "3600"],
        ["--max-llm-calls", "120"],
        ["--max-jev-calls", "500"],
        ["--agent-budget", "1"],
        ["--campaign-budget-only"],
    ],
)
def test_removed_cap_flags_are_refused_before_creating_output(tmp_path, stubbed, flags, capsys):
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, *flags)))
    assert "was removed" in capsys.readouterr().err
    assert not (tmp_path / "out").exists()
    assert stubbed == []


def test_official_agent_runs_uncapped(tmp_path, monkeypatch):
    seen, _ = patch_run(monkeypatch, b"after", {"score": 1.0})
    task = {"task_id": "b1", "confirmed_task": "go"}
    row = asyncio.run(official.execute_task(run_args(tmp_path, Suite.BU_BENCH), task, 0))
    assert seen["limits"] == Limits()
    assert row["answer"] == "done" and row["final_url"] == "https://shop.example/done"


def test_official_manifest_records_the_uncapped_protocol(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path))) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["agent_limits"] == Limits().model_dump(mode="json")
    assert manifest["budget_policy"] == "start-gate-v1"
    assert (manifest["max_steps"], manifest["task_timeout"], manifest["agent_budget"]) == (None, None, None)
    assert manifest["reserve_dollars"] == 0.5 and manifest["browser_reserve_dollars"] == 0.5
    assert "worker_budgets_usd" not in manifest
    assert any("4 hour" in note for note in manifest["provider_limits"])


def test_each_finished_slot_prints_its_facts_and_no_task_text(tmp_path, stubbed, capsys):
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10"))) == 0
    lines = [line for line in capsys.readouterr().out.splitlines() if line.startswith("RESULT ")]
    finished = rows(tmp_path / "out" / "results.jsonl")
    assert len(lines) == len(finished)
    for row in finished:
        line = next(line for line in lines if line.startswith(f"RESULT {row['task']}-{row['repeat']} "))
        assert f"score={row['upstream_score']} " in line and line.endswith("cause=None kind=None")


def test_an_image_is_labelled_by_its_own_bytes_and_nothing_else_changes():
    from fastbrowse_evals.adapters.browser_official_bridge import labelled_as_sent

    jpeg = base64.b64encode(b"\xff\xd8\xff\xe0").decode()
    png = base64.b64encode(b"\x89PNG\r\n").decode()

    def message(label, data):
        return [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "t"},
                    {"type": "image_url", "image_url": {"url": f"data:image/{label};base64,{data}", "detail": "high"}},
                ],
            }
        ]

    assert labelled_as_sent(message("png", jpeg)) == message("jpeg", jpeg)
    assert labelled_as_sent(message("png", png)) == message("png", png)


def test_result_line_names_a_failure_by_type_alone():
    row = {"task": "t", "repeat": 0, "upstream_score": None, "error_type": "ValueError", "answer": "private"}
    assert official.result_line(row, 0.5) == (
        "RESULT t-0 status=None score=None seconds=None dollars=0.500 cause=ValueError kind=None"
    )


def test_failure_kind_comes_from_the_run_failed_event():
    assert official.failure_kind([{"event": "route"}, {"event": "run_failed", "kind": "Llm.Timeout"}]) == "Llm.Timeout"
    assert official.failure_kind([{"event": "route"}]) is None


def test_every_attempt_records_uncapped_limits(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10"))) == 0
    for row in rows(tmp_path / "out" / "attempts.jsonl"):
        assert row["agent_limits"] == Limits().model_dump(mode="json") and row["reserved_usd"] == 1.5


def test_an_overrun_is_recorded_and_dispatch_continues_while_a_hold_fits(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)
    state["hold"] = False
    charges = {"t0": 6.0}

    async def execute(args, task, repeat):
        state["calls"].append((task["task_id"], repeat))
        (args.out / f"{task['task_id']}-{repeat}").mkdir()
        return {"upstream_score": 1.0, "agent_dollars": charges.get(task["task_id"], 0.2), "agent_cost_unknown": False}

    monkeypatch.setattr(official, "execute_task", execute)
    # t0 costs four times its 1.5 hold; 6 + 1.5 still fits 10, so t1 and t2 start as usual.
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10"))) == 0
    assert state["calls"] == [("t0", 0), ("t1", 0), ("t2", 0)]
    done = {r["task"]: r for r in rows(tmp_path / "out" / "results.jsonl")}
    assert done["t0"]["agent_dollars"] == 6.0 and "reservation_exceeded" not in done["t0"]


def test_the_budget_stops_only_new_tasks_once_no_hold_fits(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)
    state["hold"] = False

    async def execute(args, task, repeat):
        state["calls"].append((task["task_id"], repeat))
        (args.out / f"{task['task_id']}-{repeat}").mkdir()
        return {"upstream_score": 1.0, "agent_dollars": 9.0, "agent_cost_unknown": False}

    monkeypatch.setattr(official, "execute_task", execute)
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "10"))) == 1
    assert state["calls"] == [("t0", 0)]  # 9 + 1.5 no longer fits: t1 never starts, t0 was never stopped


def test_explicit_local_browser_ignores_configured_cloud_key(monkeypatch, tmp_path):
    settings(BROWSER_USE_API_KEY="sk-browser")
    identity = {"mode": "local-chrome", "version": "Chrome 153", "sha256": "a" * 64}
    monkeypatch.setattr(official, "local_browser_identity", lambda binary: identity)
    browser, runtime = official.browser_plan(tmp_path / "chrome")
    assert "browser_api_key" not in browser and runtime["browser"] == identity


def test_zero_browser_reserve_requires_explicit_local_official_browser(tmp_path, stubbed):
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--browser-reserve-dollars", "0")))


# --- other arms on the official suites ---


def hosted_result(final: bytes | None) -> dict[str, Any]:
    return {
        "timing": {"seconds": 4.0, "timing_basis": TIMING_BASIS, "session_open_seconds": 1.0},
        "actions": [{"index": 0, "operation": "goto", "outcome": "executed"}, {"index": 1, "operation": "reply"}],
        "answer": "The price is $5.",
        "final_url": "https://shop.example/cart",
        "screenshots": [],
        "final_frame": final,
        "fields": {"status": "idle", "completed": True, "agent_dollars": 0.3, "agent_cost_unknown": False},
        "cost_note": "hosted",
    }


@pytest.mark.parametrize("arm", ["browser-use"])
def test_a_hosted_arm_gets_the_same_mind2web_judge_request(tmp_path, monkeypatch, arm):
    _, requests = patch_run(monkeypatch, None, {"score": 1.0, "passed": True})
    asked: list[str] = []

    async def hosted(_http: Any, goal: str, folder: Path, *_: Any) -> dict[str, Any]:
        asked.append(goal)
        frame = folder / "step-0.png"
        frame.write_bytes(b"step")
        return {**hosted_result(b"final"), "screenshots": [str(frame)]}

    monkeypatch.setattr(official, "hosted_official", hosted)
    args = run_args(tmp_path, Suite.MIND2WEB)
    args.arm = arm
    task = {"task_id": "t1", "confirmed_task": "Find the price.", "website": "https://shop.example/"}
    row = asyncio.run(official.execute_task(args, task, 0))
    # The agent is told the start site the official row names, as every live arm is told its start.
    assert asked == ["Start at https://shop.example/. Find the price."]
    judge = requests[-1]
    assert [Path(p).name for p in judge["screenshots"]] == ["step-0.png", "final.png"]
    assert [json.loads(a)["operation"] for a in judge["actions"]] == ["goto", "reply"]
    assert row["arm"] == arm and row["upstream_score"] == 1.0 and row["seconds"] == 4.0
    # The results row carries what the agent answered and where it ended, not only agent.json.
    assert row["answer"] == "The price is $5." and row["final_url"] == "https://shop.example/cart"
    assert [json.loads(line)["index"] for line in (args.out / "t1-0" / "steps.jsonl").read_text().splitlines()] == [
        0,
        1,
    ]


def test_a_hosted_bu_bench_run_tells_the_judge_no_files_were_collected(tmp_path, monkeypatch):
    _, requests = patch_run(monkeypatch, None, {"score": 0.5, "verdict": None})

    async def hosted(*_: Any) -> dict[str, Any]:
        return hosted_result(None)

    monkeypatch.setattr(official, "hosted_official", hosted)
    args = run_args(tmp_path, Suite.BU_BENCH)
    args.arm = "browser-use"
    row = asyncio.run(official.execute_task(args, {"task_id": "b1", "confirmed_task": "go"}, 0))
    trace = requests[-1]["trace"]
    assert trace["final_result"] == "The price is $5." and trace["screenshots_b64"] == []
    assert any("not collected" in note for note in trace["evidence_notes"])
    assert any("No final page capture" in note for note in trace["evidence_notes"])
    assert row["final_capture_available"] is False


OPENROUTER = "https://openrouter.ai/api/v1"
REQUIRED = ["--upstream-root", "r", "--upstream-commit", "c", "--judge-python", "p", "--dataset", "d"]
REQUIRED += ["--dataset-sha256", "s", "--out", "o"]


def test_the_official_judge_is_deepseek_through_openrouter_by_default():
    args = official.build_parser().parse_args(["bu-bench", *REQUIRED])
    assert (args.judge_model, args.judge_base_url) == ("deepseek/deepseek-v4.1-flash", OPENROUTER)


def test_an_unknown_charge_commits_at_least_what_it_states() -> None:
    row = {"agent_dollars": 2.5, "agent_cost_unknown": True}
    assert official.agent_usd(row, 1.0) == 2.5
    assert official.agent_usd({"agent_dollars": 0.2, "agent_cost_unknown": True}, 1.0) == 1.0
    assert official.agent_usd({"agent_dollars": 0.2, "agent_cost_unknown": False}, 1.0) == 0.2
    assert official.interruption_usd({"interruptions": [row]}, 1.0) == 2.5


def test_an_arm_without_an_official_run_is_refused(tmp_path, stubbed):
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--arm", "browser-use-ultrafast")))


def test_the_hosted_agent_is_kept_alive_for_its_final_page_then_stopped(tmp_path, monkeypatch):
    import browser_use_sdk.v3

    from fastbrowse_evals import final_page, live

    session = SimpleNamespace(
        id="s1",
        status=SimpleNamespace(value="idle"),
        output="$5",
        model="bu-max",
        total_cost_usd=None,
        created_at=datetime(2026, 1, 1, tzinfo=UTC),
    )
    stopped = SimpleNamespace(**{**vars(session), "total_cost_usd": "0.42"})
    message = SimpleNamespace(
        id="m1",
        type="assistant_message",
        data="{}",
        screenshot_url="https://shot/1",
        created_at=session.created_at,
    )
    asked: dict[str, Any] = {}
    stop = AsyncMock(return_value=stopped)

    class Run:
        session_id = "s1"

        def __await__(self) -> Any:
            async def finish() -> Any:
                return SimpleNamespace(session=session, output="$5")

            return finish().__await__()

    class Client:
        def __init__(self, **_: Any) -> None:
            gets = iter([session, stopped])
            self.sessions = SimpleNamespace(
                get=AsyncMock(side_effect=lambda _: next(gets)),
                stop=stop,
                messages=AsyncMock(return_value=SimpleNamespace(messages=[message], has_more=False)),
            )
            browser = SimpleNamespace(cdp_url="wss://cdp/s1", agent_session_id="s1", started_at=session.created_at)
            listed = SimpleNamespace(items=[browser])
            self.browsers = SimpleNamespace(list=AsyncMock(return_value=listed))

        def run(self, goal: str, **kwargs: Any) -> Run:
            asked.update(kwargs, goal=goal)
            return Run()

    async def capture(cdp: str, *, url: str | None = None) -> bytes:
        return b"final" if cdp == "wss://cdp/s1" and url == "https://shop.example/cart" else b""

    async def observe(cdp: str) -> final_page.FinalPage:
        return final_page.FinalPage(url="https://shop.example/cart")

    monkeypatch.setattr(browser_use_sdk.v3, "AsyncBrowserUse", Client)
    monkeypatch.setattr(final_page, "capture_browser", capture)
    monkeypatch.setattr(final_page, "observe_browser", observe)
    monkeypatch.setattr(official.backend, "environment", lambda: {"BROWSER_USE_API_KEY": "bu_key"})
    transport = httpx.MockTransport(lambda request: httpx.Response(200, content=b"shot"))
    folder = tmp_path / "t1-0"
    folder.mkdir()
    out = asyncio.run(official.hosted_official(httpx.AsyncClient(transport=transport), "go", folder))
    assert asked["keep_alive"] is True and asked["max_cost_usd"] == live.HOSTED_UNCAPPED_USD
    assert out["final_frame"] == b"final" and [Path(p).read_bytes() for p in out["screenshots"]] == [b"shot"]
    stop.assert_awaited_once_with("s1")
    assert out["fields"]["agent_dollars"] == 0.42 and out["fields"]["model"] == "bu-max"
    assert out["answer"] == "$5" and out["fields"]["completed"] is True
    assert out["final_url"] == "https://shop.example/cart"

    # A session the stop did not reach keeps accruing, so whatever it read then is not its charge.
    stop.side_effect = httpx.ConnectError("down")
    out = asyncio.run(official.hosted_official(httpx.AsyncClient(transport=transport), "go", folder))
    assert out["fields"]["agent_dollars"] is None and out["fields"]["agent_cost_unknown"] is True


def test_a_refused_start_releases_its_hold_and_the_next_task_starts(tmp_path, monkeypatch):
    """A provider that refused to start the run (401 on a bad key) billed nothing, so its hold is released; a kept
    hold would have stopped the next task under this 1.5 budget."""
    state = concurrent_stub(monkeypatch, tmp_path)
    state["hold"] = False

    async def execute(args, task, repeat):
        state["calls"].append((task["task_id"], repeat))
        (args.out / f"{task['task_id']}-{repeat}").mkdir()
        raise NotStarted("browser agent run returned HTTP 401")

    monkeypatch.setattr(official, "execute_task", execute)
    assert asyncio.run(official.main(main_args(tmp_path, "--execute", "--budget", "1.5"))) == 1
    assert state["calls"] == [("t0", 0), ("t1", 0), ("t2", 0)]
    assert {r["started"] for r in rows(tmp_path / "out" / "results.jsonl")} == {False}
    journal = [r["committed"] for r in rows(tmp_path / "out" / official.JOURNAL)]
    assert journal == [1.5, 0, 1.5, 0, 1.5, 0]


def test_task_id_runs_a_named_subset_of_the_loaded_set(tmp_path, stubbed):
    argv = main_args(tmp_path, "--task-id", "t2", "--task-id", "t0", "--execute", "--budget", "10")
    assert asyncio.run(official.main(argv)) == 0
    assert stubbed == [("t0", 0), ("t2", 0)]
    assert json.loads((tmp_path / "out" / "manifest.json").read_text())["task_ids"] == ["t0", "t2"]


def test_task_id_outside_the_loaded_set_is_refused(tmp_path, stubbed, capsys):
    with pytest.raises(SystemExit):
        asyncio.run(official.main(main_args(tmp_path, "--task-id", "t0", "--task-id", "t9")))
    assert "outside the official online-mind2web set: t9" in capsys.readouterr().err
    assert stubbed == []


def interruption(passed: bool, cost: float = 0.01) -> dict[str, Any]:
    decision = {"passed": passed, "source": "llm", "reason": "the site served an error page" if not passed else "ok"}
    return {
        "judgment": {
            "policy": official.INTERRUPTION_POLICY,
            "state_sha256": "0" * 64,
            "questions": official.INTERRUPTION_QUESTIONS,
            "decisions": {"evidence_available": decision},
        },
        "policy_sha256": "f" * 64,
        "judge_cost_usd": cost,
        "known_judge_cost_usd": cost,
        "unpriced_judge_attempts": 0,
    }


@pytest.mark.real_interruption_check
@pytest.mark.parametrize("interrupted", [1, official.OUTAGE_RETRIES + 1])
def test_a_site_interrupted_run_is_run_again_never_sent_upstream(tmp_path, monkeypatch, interrupted):
    _, requests = patch_run(monkeypatch, b"after", {"score": 1.0, "passed": True})
    checks: list[dict[str, Any]] = []

    async def check(task: dict[str, Any], **seen: Any) -> dict[str, Any]:
        checks.append(seen)
        return interruption(len(checks) > interrupted)

    monkeypatch.setattr(official, "interruption_check", check)
    monkeypatch.setattr(official, "retry_wait", lambda _: 0)
    args = run_args(tmp_path, Suite.MIND2WEB)
    args.budget, args.concurrency = 100.0, 1
    task = {"task_id": "t1", "confirmed_task": "do it", "website": "https://shop.example/", "level": "easy"}
    incomplete = asyncio.run(official.run_attempts(args, [(task, 0)], 0.0, 1.5))
    [row] = rows(args.out / "results.jsonl")
    kept = row.get("interruptions", [])
    assert [r["folder"] for r in kept] == [f"t1-0.interrupted-{n}" for n in range(min(interrupted, 5))]
    assert all(r["reason"] == "the site served an error page" and r["judge_usd"] == 0 for r in kept)
    assert all((args.out / r["folder"] / "interruption.json").is_file() for r in kept)
    if interrupted <= official.OUTAGE_RETRIES:
        # Only the uninterrupted run reached the upstream judge, and every check's charge counts.
        assert len(requests) == 1 and row["upstream_score"] == 1.0 and not incomplete
        assert official.interruption_usd(row, 1.0) == pytest.approx(0.01 * (interrupted + 1) + 0.1 * interrupted)
    else:
        # Still interrupted after every retry: blocked by the site, so settled, and never the agent's failure.
        assert requests == [] and row["upstream_score"] is None and row["error_type"] == "Interrupted"
        assert official.blocked_by_site(row) and not incomplete


def waiting_stub(monkeypatch, tmp_path, *, retries: dict[str, int], charge: float = 0.0, wait: float = 0.0):
    """`execute_task` raising `Interrupted` for a task's first `retries[task]` runs, and a real out directory."""
    args = run_args(tmp_path, Suite.MIND2WEB)
    args.budget, args.concurrency = 100.0, 1
    calls: list[str] = []

    async def fake_execute(run_args: Any, task: dict[str, Any], repeat: int) -> dict[str, Any]:
        name = task["task_id"]
        calls.append(name)
        (run_args.out / f"{name}-{repeat}").mkdir()
        fields = {"status": "complete", "agent_dollars": charge, "agent_cost_unknown": False, "seconds": 1.0}
        if calls.count(name) <= retries.get(name, 0):
            raise official.Interrupted("blocked", {"judge_cost_usd": 0.0}, fields)
        return {**fields, "upstream_score": 1.0}

    monkeypatch.setattr(official, "execute_task", fake_execute)
    monkeypatch.setattr(official, "retry_wait", lambda _: wait)
    return args, calls


def named(*names: str) -> list[tuple[dict[str, Any], int]]:
    return [({"task_id": name}, 0) for name in names]


def test_the_retry_schedule_is_unchanged():
    assert [official.retry_wait(n) for n in range(official.OUTAGE_RETRIES)] == [60, 120, 240, 480, 600]


def test_a_waiting_slot_frees_its_worker_for_other_tasks(tmp_path, monkeypatch):
    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 1}, wait=0.05)
    execute = official.execute_task
    now = [0.0]

    async def complete_before_retry(run_args, task, repeat):
        result = await execute(run_args, task, repeat)
        if task["task_id"] == "c":
            now[0] = 0.05
        return result

    async def scenario():
        # Slow result writes must not turn this waiting retry into the due retry tested separately below.
        monkeypatch.setattr(asyncio.get_running_loop(), "time", lambda: now[0])
        monkeypatch.setattr(official, "execute_task", complete_before_retry)
        return await official.run_attempts(args, named("a", "b", "c"), 0.0, 1.5)

    assert asyncio.run(scenario()) is False
    assert calls == ["a", "b", "c", "a"]
    by_task = {r["task"]: r for r in rows(args.out / "results.jsonl")}
    assert [r["folder"] for r in by_task["a"]["interruptions"]] == ["a-0.interrupted-0"]
    assert by_task["a"]["interruptions"][0]["wait"] == 0.05 and by_task["a"]["upstream_score"] == 1.0
    assert sorted(r["task"] for r in rows(args.out / "attempts.jsonl")) == ["a", "b", "c"]


def test_a_due_retry_runs_ahead_of_tasks_not_yet_begun(tmp_path, monkeypatch):
    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 2})
    assert asyncio.run(official.run_attempts(args, named("a", "b"), 0.0, 1.5)) is False
    assert calls == ["a", "a", "a", "b"]


def test_every_physical_run_holds_and_settles_on_its_own_so_the_journal_replays(tmp_path, monkeypatch):
    from fastbrowse_evals import publication

    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 2}, charge=0.25)
    args.budget = 10.0
    reserve = 1.5
    assert asyncio.run(official.run_attempts(args, named("a", "b"), 0.0, reserve)) is False
    journal = rows(args.out / official.JOURNAL)
    runs = {"a-0": 0.25, "a-0.retry-1": 0.25, "a-0.retry-2": 0.25, "b-0": 0.25}
    assert [r["id"] for r in journal[:6]] == ["a-0", "a-0", "a-0.retry-1", "a-0.retry-1", "a-0.retry-2", "a-0.retry-2"]
    publication.replay_start_gate(journal, runs, 10.0, reserve, "online-mind2web")
    # The slot's whole commitment is what a resume reads back, split or not.
    a = next(r for r in rows(args.out / "results.jsonl") if r["task"] == "a")
    assert official.attempt_usd(args.out / "a-0", a, 1.0, reserve) == pytest.approx(0.75)


def test_a_waiting_slot_holds_nothing_so_the_budget_cannot_stall_on_it(tmp_path, monkeypatch):
    """One hold at a time fits. Held through its wait, `a` would keep `b` out for as long as it waited."""
    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 1}, wait=0.05)
    args.budget = 1.6
    assert asyncio.run(official.run_attempts(args, named("a", "b"), 0.0, 1.5)) is False
    assert calls == ["a", "b", "a"]


def test_a_retry_passes_the_start_gate_and_an_idle_ledger_stops_the_run(tmp_path, monkeypatch):
    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 1}, charge=0.5)
    args.budget = 1.6
    assert asyncio.run(official.run_attempts(args, named("a"), 0.0, 1.5)) is True
    assert calls == ["a"]
    assert not (args.out / "results.jsonl").exists()
    assert len(rows(args.out / "attempts.jsonl")) == 1


def test_no_worker_exits_while_a_waiting_slot_remains(tmp_path, monkeypatch):
    """`a` waits with the queue empty; its worker must stay, since `b`'s worker is busy until `a` has run again."""
    args, calls = waiting_stub(monkeypatch, tmp_path, retries={"a": 1}, wait=0.01)
    args.concurrency = 2
    interrupted = official.execute_task
    retried = asyncio.Event()

    async def fake_execute(run_args: Any, task: dict[str, Any], repeat: int) -> dict[str, Any]:
        if task["task_id"] == "b":
            (run_args.out / "b-0").mkdir()
            await retried.wait()
            return {"status": "complete", "agent_dollars": 0.0, "agent_cost_unknown": False, "upstream_score": 1.0}
        if calls.count("a") == 1:
            retried.set()
        return await interrupted(run_args, task, repeat)

    monkeypatch.setattr(official, "execute_task", fake_execute)

    async def scenario() -> bool:
        return await asyncio.wait_for(official.run_attempts(args, named("a", "b"), 0.0, 1.5), 5)

    assert asyncio.run(scenario()) is False
    assert sorted(r["task"] for r in rows(args.out / "results.jsonl")) == ["a", "b"]


def test_a_run_stopped_during_a_wait_resumes_as_a_started_attempt(tmp_path, monkeypatch):
    state = concurrent_stub(monkeypatch, tmp_path)
    state["hold"] = False
    out = tmp_path / "out"
    finished = official.execute_task

    async def interrupted_first(args: Any, task: dict[str, Any], repeat: int) -> dict[str, Any]:
        if task["task_id"] == "t0":
            (args.out / "t0-0").mkdir()
            state["calls"].append(("t0", 0))
            raise official.Interrupted("blocked", {"judge_cost_usd": 0.0}, {"agent_dollars": 0.1})
        return await finished(args, task, repeat)

    monkeypatch.setattr(official, "execute_task", interrupted_first)

    async def scenario() -> None:
        run = asyncio.create_task(
            official.main(main_args(tmp_path, "--execute", "--budget", "10", "--concurrency", "2"))
        )
        while len(rows(out / "results.jsonl") if (out / "results.jsonl").exists() else []) < 2:
            await asyncio.sleep(0.001)
        await settle()
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run

    asyncio.run(scenario())
    assert sorted(r["task"] for r in rows(out / "attempts.jsonl")) == ["t0", "t1", "t2"]
    assert sorted(r["task"] for r in rows(out / "results.jsonl")) == ["t1", "t2"]
    assert (out / "t0-0.interrupted-0").is_dir() and not (out / "t0-0").exists()

    argv = main_args(tmp_path, "--execute", "--budget", "10", "--concurrency", "2", "--resume")
    state["calls"].clear()
    assert (
        asyncio.run(official.main(argv)) == 1
    )  # started, so never repeated, and left incomplete as any stop leaves it
    assert state["calls"] == []


def test_a_sample_is_spaced_evenly_across_the_fixed_order():
    tasks = [{"task_id": f"t{i}"} for i in range(10)]
    assert [t["task_id"] for t in official.spaced_sample(tasks, 4)] == ["t0", "t2", "t5", "t7"]
    assert official.spaced_sample(tasks, 10) == tasks


def test_a_sampled_run_records_its_sample_and_refuses_what_it_cannot_take(tmp_path, stubbed):
    assert asyncio.run(official.main(main_args(tmp_path, "--sample", "2"))) == 0
    manifest = json.loads((tmp_path / "out" / "manifest.json").read_text())
    assert manifest["sample"] == 2 and len(manifest["task_ids"]) == 2
    for n, bad in enumerate((["--sample", "0"], ["--sample", "99"], ["--sample", "1", "--task-id", "t0"])):
        with pytest.raises(SystemExit):
            asyncio.run(official.main(main_args(tmp_path / str(n), *bad)))


def test_only_a_slot_the_site_blocked_is_settled_without_a_grade():
    assert not official.unsettled({"upstream_score": None, "error_type": "Interrupted"})
    assert not official.unsettled({"upstream_score": 0.0})
    assert official.unsettled({"upstream_score": None, "error_type": "ValueError"})
    assert official.unsettled(None)


@pytest.mark.real_interruption_check
async def test_the_interruption_check_asks_the_site_failure_question_of_the_final_capture(monkeypatch):
    seen: dict[str, Any] = {}

    async def judge(state: dict[str, Any], **kwargs: Any) -> Any:
        seen.update(state=state, **kwargs)
        from parallax.judging import Decision, Judgment

        decisions = {"evidence_available": Decision(passed=False, source="llm", reason="error page")}
        return Judgment(policy=kwargs["policy"], state_sha256="0" * 64, questions={}, decisions=decisions), "f" * 64

    monkeypatch.setattr(official, "judge_browser", judge)
    receipt = await official.interruption_check(
        {"task_id": "t", "confirmed_task": "find a flight"},
        answer=None,
        final_url="https://site/",
        actions=[{"index": 0}],
        final_frame=b"png",
        fields={"status": "stopped", "error": "net::ERR_NAME_NOT_RESOLVED"},
    )
    assert seen["questions"]["evidence_available"] == judging.SITE_FAILURE_QUESTIONS["evidence_available"]
    assert seen["use_jev"] is False and seen["policy"] == official.INTERRUPTION_POLICY
    assert seen["state"]["error"] == "net::ERR_NAME_NOT_RESOLVED"
    assert seen["state"]["final_page_capture"].startswith("data:image/png;base64,")
    assert official.judged_unavailable(receipt)


def test_a_settled_proxy_replaces_the_stops_reading_once_and_an_unsettled_one_is_a_floor() -> None:
    lines = (
        CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.01),
        CostLine(component=CostComponent.BROWSER, basis=CostBasis.METERED, dollars=0.02),
        CostLine(component=CostComponent.PROXY, basis=CostBasis.UNSETTLED, dollars=0.0),
    )
    result = SimpleNamespace(
        status=SimpleNamespace(value="complete"),
        cost=CostBreakdown(lines=lines),
    )
    unsettled = official.agent_fields(result)
    assert unsettled["agent_cost_unknown"] and unsettled["agent_dollars"] == pytest.approx(0.03)
    assert official.agent_usd(unsettled, 0.5) == 0.5


def test_mind2web_judge_asks_again_at_the_ceiling_when_512_tokens_cut_the_answer():
    sent = []
    replies = iter([("length", "Score: "), ("stop", "Score: 5")])

    def create(**kwargs):
        sent.append(kwargs["max_tokens"])
        finish, text = next(replies)
        message = SimpleNamespace(content=text)
        return SimpleNamespace(choices=[SimpleNamespace(finish_reason=finish, message=message)])

    client = SimpleNamespace(chat=SimpleNamespace(completions=SimpleNamespace(create=create)))
    model = SimpleNamespace(model="deepseek/deepseek-v4.1-flash", client=client)
    request = {"billing": {"max_output_tokens": 8192}}
    assert bridge_module.uncut_generate(model, request)([]) == ["Score: 5"]
    assert sent == [512, 8192]
