from decimal import Decimal
from pathlib import Path

import httpx
import pytest

from fastbrowse_evals.request_session import RequestSession, RequestSessionSpec

from .test_request_quotes import profile


def spec() -> RequestSessionSpec:
    return RequestSessionSpec(
        decision=profile(endpoint="https://openrouter.ai/api/v1/systemone", credential_id="OPENROUTER_API_KEY"),
        text=profile(
            shape="chat_completions",
            endpoint="https://openrouter.ai/api/v1/chat/completions",
            credential_id="OPENROUTER_API_KEY",
        ),
        judge=profile(
            shape="chat_completions",
            endpoint="https://openrouter.ai/api/v1/chat/completions",
            model="deepseek/deepseek-v4.1-flash-1017",
            credential_id="PARALLAX_JUDGE_API_KEY",
        ),
    )


@pytest.mark.asyncio
async def test_rust_route_and_canonical_judge_share_one_durable_wallet(tmp_path: Path) -> None:
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return httpx.Response(200, json={"usage": {"cost": 0.0001}})

    session = RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    )
    with session:
        settings = session.agent_settings()
        assert "agent-owned" not in str(settings)
        assert settings["FASTBROWSE_LLM_BASE_URL"].startswith("http://127.0.0.1:")
        async with httpx.AsyncClient(trust_env=False) as client:
            response = await client.post(
                settings["FASTBROWSE_LLM_BASE_URL"] + "/chat/completions",
                json={"model": "fixture/decision", "messages": [{"role": "user", "content": "Read."}]},
            )
            assert response.status_code == 200
        async with session.judge_client() as client:
            response = await client.post(
                "https://openrouter.ai/api/v1/chat/completions",
                json={
                    "model": "deepseek/deepseek-v4.1-flash-1017",
                    "messages": [{"role": "user", "content": "Grade."}],
                },
            )
            assert response.status_code == 200
        assert session.wallet.committed == Decimal("0.0002")
        assert len(session.wallet.charges) == len(calls) == 2
        assert calls[1].url.host == "openrouter.ai"


@pytest.mark.asyncio
async def test_judge_retry_is_a_second_physical_reservation(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import json

    from fastbrowse_evals.judging import JUDGE_MODEL, judge_browser

    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", JUDGE_MODEL)
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", "https://openrouter.ai/api/v1")
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "caller-must-not-forward")
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        assert request.headers["authorization"] == "Bearer judge-owned"
        if len(calls) == 1:
            return httpx.Response(503, json={"usage": {"cost": 0.0001}, "error": "retry"})
        answer = json.dumps({"items": [{"q": "outcome", "yes": True, "why": "Recorded evidence."}]})
        return httpx.Response(
            200,
            json={
                "id": "mock-generation",
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost": 0.0001},
                "choices": [{"message": {"content": answer}, "finish_reason": "stop"}],
            },
        )

    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    ) as session:
        async with session.judge_client() as client:
            judgment, _ = await judge_browser(
                {"evidence": "Recorded evidence."},
                policy="offline",
                questions={"outcome": "Was it done?"},
                use_jev=False,
                client=client,
            )
        assert judgment.decisions["outcome"].passed is True
        assert len(calls) == len(session.wallet.charges) == len(session.judge_cassette.exchanges) == 2
        assert session.wallet.committed == Decimal("0.0002")


@pytest.mark.asyncio
async def test_judge_exhaustion_keeps_complete_agent_evidence(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse_evals import live
    from fastbrowse_evals.live_tasks import TASKS, Outcome, PageEvidence

    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", "deepseek/deepseek-v4.1-flash-1017")
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", "https://openrouter.ai/api/v1")
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "caller-must-not-forward")
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return httpx.Response(200, json={"usage": {"cost": 0.0001}})

    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("0.001"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    ) as session:

        async def agent(*args: object, **kwargs: object):
            async with httpx.AsyncClient(trust_env=False) as client:
                await client.post(
                    session.agent_settings()["FASTBROWSE_LLM_BASE_URL"] + "/chat/completions",
                    json={"model": "fixture/decision", "messages": [{"role": "user", "content": "Read."}]},
                )
            return Outcome(
                "completed answer",
                {"result": 42},
                "https://example.test",
                evidence=PageEvidence(text="Recorded evidence."),
            ), live.ArmReport(status="complete", seconds=1, dollars=0.0001)

        monkeypatch.setattr(live, "_fast_report", agent)
        async with httpx.AsyncClient() as http:
            row = await live.run_arm(
                "fastbrowse",
                next(task for task in TASKS if task.id == "wiki-open"),
                None,
                http,
                tmp_path,
                record=None,
                request_session=session,
            )
        assert row.status == "complete"
        assert row.answer == "completed answer" and row.data == {"result": 42}
        assert row.passed is None and row.correct is None and row.semantic_judge is None
        assert row.execution_admission_stopped is True
        assert "approved total" in row.execution_admission_reason
        assert len(calls) == len(session.wallet.charges) == 1


@pytest.mark.asyncio
async def test_cancelled_judge_keeps_unknown_hold_and_drained_receipt_hashes(tmp_path: Path) -> None:
    import asyncio
    import hashlib
    import json

    began = asyncio.Event()

    async def upstream(request: httpx.Request) -> httpx.Response:
        began.set()
        await asyncio.Event().wait()
        raise AssertionError("cancelled transport resumed")

    journal = tmp_path / "wallet"
    with RequestSession(
        spec(),
        journal,
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    ) as session:
        async with session.judge_client() as client:
            pending = asyncio.create_task(
                client.post(
                    spec().judge.endpoint,
                    json={"model": spec().judge.model, "messages": [{"role": "user", "content": "Grade."}]},
                )
            )
            await asyncio.wait_for(began.wait(), 5)
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
        assert session.wallet.committed == Decimal("0.001")
        assert len(session.wallet.charges) == 1 and session.wallet.charges[0].actual is None
        assert client.partial_spend == (0.0, 1)
    evidence = json.loads(journal.with_suffix(".evidence.json").read_text())
    assert evidence["budget_policy"] == "paid-call-reservations-v1"
    for name, digest in evidence["artifacts"].items():
        assert hashlib.sha256((tmp_path / name).read_bytes()).hexdigest() == digest
    assert "owned" not in json.dumps(evidence)
    assert session.judge_cassette.exchanges == []


@pytest.mark.asyncio
async def test_concurrent_judges_cannot_oversubscribe_shared_wallet(tmp_path: Path) -> None:
    import asyncio

    from fastbrowse_evals.spend import SpendStop

    began, release = asyncio.Event(), asyncio.Event()
    calls = []

    async def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        began.set()
        await release.wait()
        return httpx.Response(200, json={"usage": {"cost": 0.0001}})

    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("0.001"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    ) as session:
        async with session.judge_client() as first, session.judge_client() as second:
            body = {"model": spec().judge.model, "messages": [{"role": "user", "content": "Grade."}]}
            pending = asyncio.create_task(first.post(spec().judge.endpoint, json=body))
            try:
                await asyncio.wait_for(began.wait(), 5)
                with pytest.raises(SpendStop, match="approved total"):
                    await second.post(spec().judge.endpoint, json=body)
            finally:
                release.set()
                await pending
        assert len(calls) == len(session.wallet.charges) == 1
        assert session.wallet.committed == Decimal("0.0001")
        assert first.partial_spend == (0.0001, 0) and second.partial_spend == (0.0, 0)


@pytest.mark.parametrize("unexpected", [False, True])
@pytest.mark.asyncio
async def test_cli_persists_exhaustion_and_separate_wallet_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, unexpected: bool
) -> None:
    import json

    from fastbrowse_evals import live

    from .test_live import _live_fakes, _row

    source = tmp_path / "profiles.json"
    source.write_text(spec().model_dump_json())
    out = tmp_path / "rows.jsonl"
    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", "deepseek/deepseek-v4.1-flash-1017")
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", "https://openrouter.ai/api/v1")
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "judge-owned")
    monkeypatch.setattr(
        live.backend,
        "environment",
        lambda: {"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
    )
    monkeypatch.setattr(live, "local_browser_identity", lambda path: {"mode": "local-chrome", "binary": str(path)})

    async def run(arm, task, *args, request_session, **kwargs):
        if unexpected:
            raise RuntimeError("offline unexpected failure")
        return _row(arm, task, 0).model_copy(
            update={
                "passed": None,
                "correct": None,
                "execution_admission_stopped": True,
                "execution_admission_reason": "approved total exhausted",
                "events": [
                    {"url": request_session.agent_settings()["FASTBROWSE_LLM_BASE_URL"] + "/chat/completions"},
                    Decimal("1.25"),
                    object(),
                ],
            }
        )

    from unittest.mock import AsyncMock

    monkeypatch.setattr(RequestSession, "verify_credit", AsyncMock())
    _live_fakes(monkeypatch, run)
    argv = [
        "--only",
        "wiki-open",
        "--arms",
        "fastbrowse",
        "--local-browser",
        str(tmp_path / "chrome"),
        "--budget",
        "1",
        "--request-profiles",
        str(source),
        "--out",
        str(out),
    ]
    if unexpected:
        with pytest.raises(RuntimeError, match="offline unexpected failure"):
            await live.main(argv)
        attempts = [json.loads(line) for line in out.with_suffix(".attempts.jsonl").read_text().splitlines()]
        assert len(attempts) == 1
        assert attempts[0]["selected"] is False and attempts[0]["unknown_cost"] is True
        assert attempts[0]["passed"] is None and "RuntimeError" in attempts[0]["failure"]
        assert out.with_suffix(".requests.evidence.json").exists()
        return
    assert await live.main(argv) == 1
    row = json.loads(out.read_text())
    assert row["answer"] == "retained completed answer"
    assert row["execution_admission_stopped"] is True and row["passed"] is None
    assert row["run"]["budget_policy"] == "paid-call-reservations-v1"
    assert row["run"]["providers"] == "jev openrouter"
    assert row["run"]["request_runtime"]["files_sha256"]
    assert all(not name.startswith("/") for name in row["run"]["request_runtime"]["files_sha256"])
    assert all(
        row["run"][field] is None for field in ("reserve_dollars", "browser_reserve_dollars", "judge_reserve_dollars")
    )
    assert row["events"][0] == {"url": "https://openrouter.ai/api/v1/chat/completions"}
    assert row["events"][1] == "1.25"
    assert isinstance(row["events"][2], str)
    assert "agent-owned" not in out.read_text() and "judge-owned" not in out.read_text()
    assert not out.with_suffix(".budget.jsonl").exists()
    evidence = out.with_suffix(".requests.evidence.json")
    assert evidence.exists()
    assert json.loads(evidence.read_text())["artifacts"]["rows.requests.jsonl"]


@pytest.mark.parametrize("arguments", [[], ["--arms", "browser-use"], ["--arms", "fastbrowse", "--budget", "1"]])
@pytest.mark.asyncio
async def test_request_profile_mode_refuses_cloud_and_unbudgeted_paths(arguments, tmp_path: Path) -> None:
    from fastbrowse_evals import live

    with pytest.raises(SystemExit):
        await live.main([*arguments, "--request-profiles", str(tmp_path / "missing.json")])


@pytest.mark.parametrize(
    "remaining,account,recorded",
    [
        ("2", ("3", "0"), True),
        ("0.5", ("3", "0"), True),
        (None, ("3", "1"), True),
        (None, ("1", "0.5"), True),
        (True, ("3", "0"), False),
        ("2", ("1", "0.5"), True),
    ],
)
@pytest.mark.asyncio
async def test_credit_preflight_records_credit_without_refusing_a_campaign(
    tmp_path: Path, remaining, account, recorded
) -> None:
    import json

    from pydantic import ValidationError

    from fastbrowse_evals.spend import SpendStop

    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        assert request.method == "GET"
        assert request.url.host == "openrouter.ai"
        calls.append(request.url.path)
        if request.url.path.endswith("/key"):
            value = remaining if isinstance(remaining, bool) or remaining is None else float(remaining)
            return httpx.Response(200, json={"data": {"limit_remaining": value}})
        assert account is not None
        return httpx.Response(
            200, json={"data": {"total_credits": float(account[0]), "total_usage": float(account[1])}}
        )

    session = RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    )
    if recorded:
        await session.verify_credit()
        receipt = (tmp_path / "wallet.credit.json").read_text()
        assert json.loads(receipt)["approved_usd"] == "1"
        assert "agent-owned" not in receipt and "judge-owned" not in receipt
    else:
        with pytest.raises((SpendStop, ValidationError)):
            await session.verify_credit()
        assert not (tmp_path / "wallet.credit.json").exists()
    assert calls and not (tmp_path / "wallet").exists()
    if recorded:
        import hashlib

        with session:
            pass
        evidence = json.loads((tmp_path / "wallet.evidence.json").read_text())
        assert evidence["artifacts"]["wallet.credit.json"] == hashlib.sha256(receipt.encode()).hexdigest()


@pytest.mark.asyncio
async def test_credit_preflight_records_lower_effective_provider_budget(tmp_path: Path) -> None:
    import json

    upstream = httpx.MockTransport(
        lambda request: httpx.Response(
            200,
            json={
                "data": {"total_credits": 3, "total_usage": 0}
                if request.url.path.endswith("/credits")
                else {"limit_remaining": 2, "effective_budget": {"remaining_usd": 0.5}}
            },
        )
    )
    session = RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: upstream,
    )
    await session.verify_credit()
    receipt = json.loads((tmp_path / "wallet.credit.json").read_text())
    assert [provider["available_usd"] for provider in receipt["providers"]] == ["0.5", "0.5"]


def test_agent_settings_pin_one_custom_route_and_strip_inherited_keys(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fastbrowse_evals import backend

    monkeypatch.setattr(
        backend,
        "environment",
        lambda: {
            "FASTBROWSE_LLM_PROVIDER": "anthropic",
            "ANTHROPIC_API_KEY": "inherited-secret",
            "FASTBROWSE_JEV_API_KEY": "legacy-secret",
            "FASTBROWSE_DECISION_SOURCE": "typesafe",
        },
    )
    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(lambda request: httpx.Response(500)),
    ) as session:
        settings = backend.run_settings(session.agent_settings())
        assert settings["FASTBROWSE_LLM_PROVIDER"] == "custom"
        assert settings["FASTBROWSE_LLM_MODEL"] == spec().text.model
        assert settings["FASTBROWSE_DECISION_SOURCE"] == "openrouter"
        assert not any(
            secret in str(settings) for secret in ("inherited-secret", "legacy-secret", "agent-owned", "judge-owned")
        )
        assert not any(key.startswith("FASTBROWSE_LLM_MODEL_") for key in settings)


@pytest.mark.parametrize("suffix", [".agent.jsonl.gz", ".judge.jsonl.gz", ".credit.json", ".evidence.json"])
def test_existing_session_evidence_is_not_overwritten(tmp_path: Path, suffix: str) -> None:
    from fastbrowse_evals.spend import SpendStop

    journal = tmp_path / "wallet.jsonl"
    prior = journal.with_suffix(suffix)
    prior.write_bytes(b"retained prior evidence")
    with pytest.raises(SpendStop, match="fresh wallet and evidence"):
        RequestSession(spec(), journal, limit=Decimal("1"), campaign="offline", credentials={})
    assert prior.read_bytes() == b"retained prior evidence"
    assert not journal.exists()


@pytest.mark.asyncio
async def test_custom_agent_trace_survives_session_sanitization(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fastbrowse_evals import live
    from fastbrowse_evals.live_tasks import TASKS, Outcome, PageEvidence

    async def agent(*args, **kwargs):
        return Outcome(
            "retained answer", {}, "https://example.test", evidence=PageEvidence(text="Recorded evidence.")
        ), live.ArmReport(status="complete", seconds=1, dollars=0, events=[object()])

    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": agent}))
    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("0.0001"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(lambda request: httpx.Response(500)),
    ) as session:
        async with httpx.AsyncClient() as http:
            row = await live.run_arm(
                "fastbrowse",
                next(t for t in TASKS if t.id == "wiki-open"),
                None,
                http,
                tmp_path,
                record=None,
                request_session=session,
            )
    assert row.answer == "retained answer" and isinstance(row.events[0], str)
    assert row.passed is None


@pytest.mark.asyncio
async def test_partial_judge_receipt_survives_retry_admission_stop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fastbrowse_evals import live
    from fastbrowse_evals.judging import judge_browser
    from fastbrowse_evals.live_tasks import TASKS, Outcome

    async def agent(*args, **kwargs):
        return Outcome("completed answer", {}, "https://example.test"), live.ArmReport(
            status="complete", seconds=1, dollars=0
        )

    async def grade(*args, client, **kwargs):
        return await judge_browser(
            {"text": "Recorded evidence."},
            policy=live.JUDGE_POLICY,
            questions=live.POLICY_QUESTIONS[live.JUDGE_POLICY],
            use_jev=False,
            client=client,
        )

    monkeypatch.setenv("PARALLAX_JUDGE_MODELS", "deepseek/deepseek-v4.1-flash-1017")
    monkeypatch.setenv("PARALLAX_JUDGE_BASE_URL", "https://openrouter.ai/api/v1")
    monkeypatch.setenv("PARALLAX_JUDGE_API_KEY", "judge-owned")
    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": agent}))
    monkeypatch.setattr(live, "semantic_grade", grade)
    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("0.001"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(
            lambda request: httpx.Response(503, json={"usage": {"cost": 0.0001}})
        ),
    ) as session:
        async with httpx.AsyncClient() as http:
            row = await live.run_arm(
                "fastbrowse",
                next(t for t in TASKS if t.id == "wiki-open"),
                None,
                http,
                tmp_path,
                record=None,
                request_session=session,
            )
        assert len(session.wallet.charges) == 1
    assert row.status == "complete" and row.answer == "completed answer"
    assert row.semantic_judge is None and row.judge_dollars is None and row.passed is None
    assert row.known_judge_dollars == pytest.approx(0.0001)
    assert row.unpriced_judge_attempts == 0


@pytest.mark.asyncio
async def test_account_credit_permission_failure_stops_before_wallet(tmp_path: Path) -> None:
    def upstream(request: httpx.Request) -> httpx.Response:
        assert request.method == "GET"
        return (
            httpx.Response(403)
            if request.url.path.endswith("/credits")
            else httpx.Response(200, json={"data": {"limit_remaining": 2}})
        )

    session = RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(upstream),
    )
    with pytest.raises(httpx.HTTPStatusError):
        await session.verify_credit()
    assert not (tmp_path / "wallet").exists() and not (tmp_path / "wallet.credit.json").exists()


@pytest.mark.parametrize("cancelled", [False, True])
@pytest.mark.asyncio
async def test_interrupted_grade_preserves_its_own_partial_receipt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, cancelled: bool
) -> None:
    import asyncio

    from fastbrowse_evals import live
    from fastbrowse_evals.live_tasks import TASKS, Outcome

    failure = asyncio.CancelledError if cancelled else RuntimeError
    task = next(t for t in TASKS if t.id == "wiki-open")

    async def agent(*args, **kwargs):
        return Outcome("completed answer", {}, "https://example.test"), live.ArmReport(
            status="complete", seconds=1, dollars=0
        )

    async def grade(*args, client, **kwargs):
        await client.post(
            spec().judge.endpoint,
            json={"model": spec().judge.model, "messages": [{"role": "user", "content": "Recorded evidence."}]},
        )
        raise failure("offline judgment interruption")

    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": agent}))
    monkeypatch.setattr(live, "semantic_grade", grade)
    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(
            lambda request: httpx.Response(200, json={"usage": {"cost": 0.0001}})
        ),
    ) as session:
        async with httpx.AsyncClient() as http:
            with pytest.raises(failure):
                await live.run_arm("fastbrowse", task, None, http, tmp_path, record=None, request_session=session)
    retained = live._crashed("fastbrowse", task, "interrupted", at=0, seconds=2, status=None, record=None)
    assert retained.answer == "completed answer" and retained.known_judge_dollars == pytest.approx(0.0001)
    assert retained.judge_dollars is None and retained.passed is None and retained.unpriced_judge_attempts == 0


@pytest.mark.parametrize("cost", ["-1", "NaN", "Infinity"])
@pytest.mark.asyncio
async def test_partial_judge_spend_keeps_invalid_receipt_unknown(tmp_path: Path, cost: str) -> None:
    with RequestSession(
        spec(),
        tmp_path / "wallet",
        limit=Decimal("1"),
        campaign="offline",
        credentials={"OPENROUTER_API_KEY": "agent-owned", "PARALLAX_JUDGE_API_KEY": "judge-owned"},
        transport_factory=lambda: httpx.MockTransport(
            lambda request: httpx.Response(200, text='{"usage":{"cost":' + cost + "}}")
        ),
    ) as session:
        async with session.judge_client() as client:
            await client.post(
                spec().judge.endpoint,
                json={"model": spec().judge.model, "messages": [{"role": "user", "content": "Read."}]},
            )
        assert client.partial_spend == (0.0, 1)
        assert session.wallet.charges[0].actual is None
        assert session.judge_cassette.exchanges[0].cost is None
