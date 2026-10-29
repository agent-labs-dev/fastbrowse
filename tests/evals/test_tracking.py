import json
from datetime import UTC, datetime
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals import storage, tracking


def row(task: str, *, repeat: int = 0, passed: bool = True, seconds: float = 5.0, **overrides: object) -> dict:
    value = {
        "run": {
            "run_id": "abc123",
            "run_started": "2026-10-29T10:00:00+00:00",
            "fastbrowse_version": "0.7.1",
            "git_sha": "a" * 40,
            "git_dirty": False,
            "providers": "Jev openrouter; LLM via OpenRouter",
        },
        "suite": "mock",
        "task": task,
        "repeat": repeat,
        "passed": passed,
        "status": "complete",
        "normalized_status": "done",
        "seconds": seconds,
        "dollars": 0.01,
        "unknown_cost": False,
        "task_version": "6",
        "suite_version": "22995d7d",
        "steps": 7,
    }
    value.update(overrides)
    return value


def rows_file(tmp_path: Path, rows: list[dict]) -> Path:
    path = tmp_path / "nightly.jsonl"
    path.write_text("".join(json.dumps(item) + "\n" for item in rows))
    return path


def attributes(span: dict) -> dict:
    return {item["key"]: next(iter(item["value"].values())) for item in span["attributes"]}


def client_for(handler) -> httpx.Client:
    return httpx.Client(base_url=storage.HOST, transport=httpx.MockTransport(handler))


def test_every_span_names_the_build_and_the_attempt(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket"), row("mock-sign-in", repeat=1)])
    body = tracking.payload(path, tracking.read_rows(path))
    spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"]

    assert len(spans) == 2
    for span in spans:
        found = attributes(span)
        assert found["langfuse.trace.metadata.build"] == "a" * 40
        assert found["langfuse.trace.metadata.run_id"] == "abc123"
        assert found["langfuse.release"] == "fastbrowse/0.7.1@aaaaaaa/nightly"
        assert found["langfuse.trace.name"].startswith("fastbrowse/0.7.1@aaaaaaa/nightly/mock/")
        assert len(span["traceId"]) == 32 and len(span["spanId"]) == 16
    assert spans[0]["traceId"] != spans[1]["traceId"]


def test_a_repeat_is_the_same_trace_and_another_campaign_is_not(tmp_path: Path) -> None:
    first, second = row("mock-basket"), row("mock-basket")
    assert tracking.trace_id("campaign", first) == tracking.trace_id("campaign", second)
    assert tracking.trace_id("campaign", first) != tracking.trace_id("other", first)
    assert tracking.trace_id("campaign", first) != tracking.trace_id("campaign", row("mock-sign-in", repeat=1))


def test_rows_mixing_two_builds_are_refused(tmp_path: Path) -> None:
    other = row("mock-sign-in")
    other["run"] = {**other["run"], "git_sha": "b" * 40}
    path = rows_file(tmp_path, [row("mock-basket"), other])
    with pytest.raises(tracking.RowsError, match="mix"):
        tracking.payload(path, tracking.read_rows(path))


def test_rows_from_a_dirty_tree_are_refused(tmp_path: Path) -> None:
    dirty = row("mock-basket")
    dirty["run"] = {**dirty["run"], "git_dirty": True}
    path = rows_file(tmp_path, [dirty])
    with pytest.raises(tracking.RowsError, match="dirty"):
        tracking.payload(path, tracking.read_rows(path))


def test_rows_without_a_build_are_refused(tmp_path: Path) -> None:
    missing = row("mock-basket")
    missing["run"] = {**missing["run"], "git_sha": ""}
    path = rows_file(tmp_path, [missing])
    with pytest.raises(tracking.RowsError, match="git_sha"):
        tracking.payload(path, tracking.read_rows(path))


def test_read_back_accepts_the_v2_index(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        return httpx.Response(200, json={"data": [{"id": "x", "name": "anything"}], "meta": {}})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=1, delay=0)
    assert seen == ["/api/public/v2/observations"]


def test_read_back_falls_back_to_the_trace_while_the_index_lags(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    rows = tracking.read_rows(path)
    wanted = tracking.expected(path, rows)
    name = next(iter(wanted.values()))
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        if request.url.path == "/api/public/v2/observations":
            return httpx.Response(200, json={"data": [], "meta": {}})
        return httpx.Response(200, json={"name": name, "observations": []})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=1, delay=0)
    assert seen == ["/api/public/v2/observations", f"/api/public/traces/{next(iter(wanted))}"]


def test_an_unreadable_trace_fails_the_upload(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/api/public/v2/observations":
            return httpx.Response(200, json={"data": [], "meta": {}})
        return httpx.Response(404, json={"message": "not found"})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)


def test_a_trace_read_back_under_another_name_does_not_count(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/api/public/v2/observations":
            return httpx.Response(200, json={"data": [], "meta": {}})
        return httpx.Response(200, json={"name": "someone-elses-run", "observations": []})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=1, delay=0)


def test_dry_run_prints_the_build_and_posts_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    monkeypatch.setattr(tracking, "publish", lambda *_a, **_k: pytest.fail("dry run must not publish"))
    assert tracking.main(["--rows", str(path), "--dry-run"]) == 0
    printed = json.loads(capsys.readouterr().out)
    assert printed == {"campaign": "fastbrowse/0.7.1@aaaaaaa/nightly", "build": "a" * 40, "traces": 1}


def test_a_refused_project_or_missing_key_is_one_named_line(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])

    def refuse(*_args: object, **_kwargs: object) -> dict:
        raise KeyError("LANGFUSE_PUBLIC_KEY")

    monkeypatch.setattr(tracking, "publish", refuse)
    assert tracking.main(["--rows", str(path)]) == 1
    assert "eval publishing failed" in capsys.readouterr().err
