import json
from datetime import UTC, datetime
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals import storage, tracking


def run(**overrides: object) -> dict:
    value = {
        "run_id": "abc123",
        "run_started": "2026-10-29T10:00:00+00:00",
        "fastbrowse_version": "0.7.1",
        "git_sha": "a" * 40,
        "git_dirty": False,
        "providers": "Jev openrouter; LLM via OpenRouter",
        # The runner embeds per-suite limits here, so these differ between the local and mock rows of one run.
        "agent_limits": {"max_steps": 40, "max_jev_calls": None},
    }
    value.update(overrides)
    return value


def row(
    task: str, *, repeat: int = 0, passed: bool = True, seconds: float = 5.0, suite: str = "mock", **overrides: object
) -> dict:
    value = {
        "run": run(agent_limits={"max_steps": 40 if suite == "mock" else 25, "max_jev_calls": None}),
        "suite": suite,
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


def stored(spec: dict, *, output: dict | None = None, run_id: str | None = None) -> dict:
    return {
        "id": "x",
        "name": "anything",
        "output": spec["output"] if output is None else output,
        "metadata": {"run_id": spec["run_id"] if run_id is None else run_id},
    }


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


def test_boundary_headers_ask_for_real_time_ingestion(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    sent: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        sent.append(request)
        if request.url.path == tracking.OTLP_PATH:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"data": [stored(next(iter(wanting.values())))], "meta": {}})

    wanting = tracking.expected(path, tracking.read_rows(path))
    monkeypatch.setattr(tracking, "connection", lambda: _context(client_for(handler)))
    tracking.publish(path, attempts=1, delay=0)

    upload = next(request for request in sent if request.url.path == tracking.OTLP_PATH)
    assert upload.headers["x-langfuse-ingestion-version"] == "4"


class _context:
    def __init__(self, client: httpx.Client) -> None:
        self.client = client

    def __enter__(self) -> httpx.Client:
        return self.client

    def __exit__(self, *_exc: object) -> None:
        self.client.close()


def test_a_repeat_is_the_same_trace_and_another_campaign_is_not() -> None:
    first, second = row("mock-basket"), row("mock-basket")
    assert tracking.trace_id("campaign", first) == tracking.trace_id("campaign", second)
    assert tracking.trace_id("campaign", first) != tracking.trace_id("other", first)
    assert tracking.trace_id("campaign", first) != tracking.trace_id("campaign", row("mock-sign-in", repeat=1))


def test_two_suites_with_different_limits_are_one_run(tmp_path: Path) -> None:
    # The nightly job runs local and mock into one rows file, and their agent limits legitimately differ.
    path = rows_file(tmp_path, [row("mock-basket"), row("search", suite="local")])
    body = tracking.payload(path, tracking.read_rows(path))
    assert len(body["resourceSpans"][0]["scopeSpans"][0]["spans"]) == 2


def test_rows_from_two_runs_are_refused(tmp_path: Path) -> None:
    other = row("mock-sign-in")
    other["run"] = {**other["run"], "run_id": "def456"}
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
    asked: list[httpx.QueryParams] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        asked.append(request.url.params)
        return httpx.Response(200, json={"data": [stored(next(iter(wanted.values())))], "meta": {}})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=1, delay=0)
    assert seen == ["/api/public/v2/observations"]
    # The index omits the run id and the output unless the field groups are named, so both must be asked for.
    assert set(asked[0]["fields"].split(",")) >= {"io", "metadata"}


def test_a_lagging_index_is_waited_for_not_read_from_a_retired_endpoint(tmp_path: Path) -> None:
    # The trace endpoint read-back used to fall back to is retired on 2026-11-16, so a span that is not in the
    # index yet must be waited for: the two answers are "not yet" and "readable", never "ask somewhere else".
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    seen: list[str] = []
    answers = [0]

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        answers[0] += 1
        if answers[0] == 1:
            return httpx.Response(200, json={"data": [], "meta": {}})
        return httpx.Response(200, json={"data": [stored(next(iter(wanted.values())))], "meta": {}})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=3, delay=0)
    assert seen == ["/api/public/v2/observations", "/api/public/v2/observations"]


def test_the_retired_trace_endpoint_is_never_asked(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket"), row("mock-sign-in")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        if request.url.path != "/api/public/v2/observations":
            return httpx.Response(200, json={"metadata": {"run_id": "abc123"}, "observations": []})
        trace = request.url.params["traceId"]
        rows = [stored(wanted[trace])] if trace == next(iter(wanted)) else []
        return httpx.Response(200, json={"data": rows, "meta": {}})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)
    assert set(seen) == {"/api/public/v2/observations"}


def test_an_older_trace_under_a_reused_id_does_not_count(tmp_path: Path) -> None:
    # A rerun reuses the trace id on purpose, so an id that already existed must not pass as this upload.
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    spec = next(iter(wanted.values()))

    def handler(request: httpx.Request) -> httpx.Response:
        stale = stored(spec, output={**spec["output"], "dollars": 9.99}, run_id="previous-run")
        return httpx.Response(200, json={"data": [stale], "meta": {}})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)


def test_a_verified_trace_is_not_asked_for_again(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket"), row("mock-sign-in")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    first, second = list(wanted)
    asked: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/api/public/v2/observations":
            trace = request.url.params["traceId"]
            asked.append(trace)
            if trace == first or asked.count(second) == 3:
                return httpx.Response(200, json={"data": [stored(wanted[trace])], "meta": {}})
            return httpx.Response(200, json={"data": [], "meta": {}})
        return httpx.Response(404, json={"message": "not found"})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=3, delay=0)
    assert asked.count(first) == 1
    assert asked.count(second) == 3


def test_a_rate_limited_read_is_waited_out(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    slept: list[float] = []
    calls: list[str] = []
    monkeypatch.setattr(tracking.time, "sleep", slept.append)

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(request.url.path)
        if len(calls) == 1:
            return httpx.Response(429, headers={"retry-after": "7"})
        return httpx.Response(200, json={"data": [stored(next(iter(wanted.values())))], "meta": {}})

    tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)
    assert slept[0] == 7.0  # the retry-after Langfuse named, not our own delay


def test_an_unreadable_trace_fails_the_upload(tmp_path: Path) -> None:
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"data": [], "meta": {}})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)


def test_a_row_without_the_run_id_does_not_count_as_this_upload(tmp_path: Path) -> None:
    # If the metadata field group were dropped from the request the index would return no run id, and every
    # row would read as a stranger's: the check has to fail closed rather than match on the output alone.
    path = rows_file(tmp_path, [row("mock-basket")])
    wanted = tracking.expected(path, tracking.read_rows(path))
    spec = next(iter(wanted.values()))

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"data": [{**stored(spec), "metadata": None}], "meta": {}})

    with pytest.raises(tracking.RowsError, match="not readable"):
        tracking.verify(client_for(handler), wanted, since=datetime(2026, 10, 29, tzinfo=UTC), attempts=2, delay=0)


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
