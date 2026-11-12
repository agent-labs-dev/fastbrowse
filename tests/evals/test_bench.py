import dataclasses
import errno
import gc
import hashlib
import json
import socket
import warnings
from contextlib import ExitStack, contextmanager
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals.bench import (
    BenchTask,
    _serve,
    agent_environment,
    compare_runs,
    corpus,
    fixture_site,
    load_run,
    priced,
    run_dollars,
    run_params,
)
from fastbrowse.evals.local import fixture_server
from fastbrowse.evals.replay import (
    EMBEDDED_JSON,
    MISS_STATUS,
    Cassette,
    Exchange,
    Proxy,
    canonical_json,
    canonical_value,
    describe_request,
    miss_diff,
    nearest_body,
    openrouter_forward,
    request_key,
    response_cost,
    translate_origin,
    translate_response_origin,
)
from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine

LLM = "/llm/chat/completions"
JEV = "/jev/v1/systemone"


@pytest.mark.parametrize("cost", [-1, float("nan"), float("inf"), float("-inf"), 10**400, True, "0.1", None])
def test_invalid_provider_costs_remain_unknown(cost: object) -> None:
    assert response_cost(json.dumps({"usage": {"cost": cost}})) is None


@pytest.mark.parametrize("cost", [0, 0.125, 2])
def test_finite_nonnegative_provider_costs_are_retained(cost: int | float) -> None:
    assert response_cost(json.dumps({"usage": {"cost": cost}})) == cost


def _exchange(path: str, request: object, body: str, *, latency: float = 0.0) -> Exchange:
    purpose, model = describe_request(path, request)
    return Exchange(
        key=request_key(path, request),
        path=path,
        purpose=purpose,
        model=model,
        request=request,
        status=200,
        content_type="application/json",
        body=body,
        latency=latency,
    )


def test_key_ignores_member_order_and_spacing() -> None:
    assert request_key(JEV, '{"a": 1, "b": [1, 2]}') == request_key(JEV, {"b": [1, 2], "a": 1})


def test_key_depends_on_path_and_values() -> None:
    assert request_key(JEV, {"a": 1}) != request_key(LLM, {"a": 1})
    assert request_key(JEV, {"a": 1}) != request_key(JEV, {"a": 2})
    assert request_key(JEV, {"a": [1, 2]}) != request_key(JEV, {"a": [2, 1]})


def test_integral_floats_are_one_number() -> None:
    assert canonical_json({"p": 1.0}) == canonical_json({"p": 1}) == '{"p":1}'
    assert canonical_json({"p": 0.25}) == '{"p":0.25}'


def test_fixture_ports_are_placeholders() -> None:
    first = {"url": "http://127.0.0.1:47301/shop.html", "text": "served at 127.0.0.1:47301"}
    second = {"url": "http://127.0.0.1:51234/shop.html", "text": "served at 127.0.0.1:51234"}
    assert request_key(JEV, first) == request_key(JEV, second)
    assert "<port>" in canonical_json(first)
    assert request_key(JEV, {"url": "http://localhost:3000/"}) == request_key(JEV, {"url": "http://localhost:4000/"})


def test_capture_dates_are_placeholders() -> None:
    def state(day: str, weekday: str, named: str) -> dict[str, object]:
        date = f"{day} ({weekday})\nThe dates the task names, worked out from today:\n4 December: {named}"
        return {"date": date, "note": f"today: {json.dumps(date)}", "captured_at": f"{day}T10:11:12.345678+00:00"}

    friday = state("2026-10-23", "Friday", "2026-12-04 (Friday)")
    monday = state("2027-02-01", "Monday", "2027-12-04 (Saturday)")
    assert request_key(LLM, friday) == request_key(LLM, monday)
    named = "<today>\nThe dates the task names, worked out from today:\n4 December: <today>"
    assert canonical_value(friday) == {
        "date": named,
        "note": f"today: {json.dumps(named)}",
        "captured_at": "<timestamp>",
    }


def test_screenshots_are_placeholders() -> None:
    def message(png: str) -> dict[str, object]:
        image = {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{png}"}}
        return {"messages": [{"role": "user", "content": [{"type": "text", "text": "page"}, image]}]}

    assert request_key(LLM, message("iVBORw0KGgoAAA")) == request_key(LLM, message("iVBORw0KGgoBBB"))
    assert "<image>" in canonical_json(message("x"))


def test_a_screenshot_jev_is_sent_as_an_asset_is_a_placeholder() -> None:
    # Jev names an image asset by its pixels' digest, so a frame painted a pixel differently missed in CI.
    def state(png: str) -> dict[str, object]:
        name = f"screenshot-{hashlib.sha256(png.encode()).hexdigest()}"
        asset = {"type": "image", "format": "png", "data_base64": png}
        return {"state": {"viewport_screenshot": name}, "assets": {name: asset}}

    assert request_key(JEV, state("iVBORw0KGgoAAA")) == request_key(JEV, state("iVBORw0KGgoBBB"))


def test_a_date_the_task_names_is_kept() -> None:
    assert request_key(LLM, {"task": "Book 2027-03-04"}) != request_key(LLM, {"task": "Book 2027-03-05"})


def test_embedded_json_is_keyed_on_the_document() -> None:
    a = {"messages": [{"role": "user", "content": json.dumps({"x": 1, "y": 2})}]}
    b = {"messages": [{"role": "user", "content": json.dumps({"y": 2, "x": 1}, indent=2)}]}
    assert request_key(LLM, a) == request_key(LLM, b)
    assert canonical_value(a)["messages"][0]["content"] == {EMBEDDED_JSON: {"x": 1, "y": 2}}
    assert canonical_value({"text": "{not json}"}) == {"text": "{not json}"}


def test_list_order_is_part_of_the_key() -> None:
    # A set written in another order is another prompt; the agent sorts `operations` so both sides agree.
    a = {"content": json.dumps({"operations": ["click", "enter", "fill"]})}
    b = {"content": json.dumps({"operations": ["fill", "click", "enter"]})}
    assert request_key(LLM, a) != request_key(LLM, b)


def test_describe_names_purpose_and_model() -> None:
    llm = {"model": "m", "response_format": {"json_schema": {"name": "Plan"}}}
    assert describe_request(LLM, llm) == ("llm:Plan", "m")
    jev = {"model": "jev-1.13", "questions": {"operation": {}, "bot_check": {}}}
    assert describe_request(JEV, jev) == ("jev:bot_check,operation", "jev-1.13")


def test_miss_diff_shows_the_changed_line() -> None:
    recorded = {"state": {"requirements": [], "task": "t"}}
    requested = {"state": {"requirements": ["Find the price."], "task": "t"}}
    diff = miss_diff(requested, recorded)
    assert '-  "requirements": [],' in diff
    assert '+   "Find the price."' in diff
    assert miss_diff(requested, None) == "(nothing recorded on this path)"


def test_nearest_body_is_the_most_similar() -> None:
    target = {"task": "t", "step": 3, "controls": ["a", "b", "c"]}
    near = {"task": "t", "step": 2, "controls": ["a", "b", "c"]}
    far = {"task": "u", "step": 9, "controls": []}
    assert nearest_body(target, [far, near]) == near
    assert nearest_body(target, []) is None


def test_cassette_round_trips_and_writes_the_same_bytes(tmp_path: Path) -> None:
    cassette = Cassette(task="t", meta={"recorded_at": "2026-10-23"})
    cassette.exchanges.append(_exchange(JEV, {"q": 1}, '{"usage": {"cost": 0.5}}', latency=1.25))
    first, second = tmp_path / "a.jsonl.gz", tmp_path / "b.jsonl.gz"
    cassette.save(first)
    cassette.save(second)
    assert first.read_bytes() == second.read_bytes()
    loaded = Cassette.load(first)
    assert loaded.task == "t" and loaded.meta == {"recorded_at": "2026-10-23"}
    assert loaded.exchanges[0].key == request_key(JEV, {"q": 1})
    assert loaded.exchanges[0].latency == 1.25


def test_loading_rekeys_from_the_request(tmp_path: Path) -> None:
    cassette = Cassette(task="t")
    stale = _exchange(JEV, {"q": 1}, "{}")
    stale.key = "stale"
    cassette.exchanges.append(stale)
    cassette.save(tmp_path / "c.jsonl.gz")
    assert Cassette.load(tmp_path / "c.jsonl.gz").exchanges[0].key == request_key(JEV, {"q": 1})


def _post(base: str, path: str, body: object, token: str = "one") -> httpx.Response:
    return httpx.post(base + path, json=body, headers={"Authorization": f"Bearer {token}"}, timeout=5)


def test_replay_ignores_the_bearer_token_and_serves_duplicates_in_order() -> None:
    cassette = Cassette(task="t")
    cassette.exchanges += [_exchange(JEV, {"q": 1}, '{"n": 1}'), _exchange(JEV, {"q": 1}, '{"n": 2}')]
    with Proxy(cassette) as base:
        served = [_post(base, JEV, {"q": 1}, token=str(i)).json()["n"] for i in range(4)]
    assert served == [1, 2, 2, 2]


def test_replay_translates_only_the_recorded_fixture_origin() -> None:
    cassette = Cassette(task="t", meta={"port": 47334})
    cassette.exchanges.append(
        _exchange(
            JEV, {"q": 1}, '{"url":"http://127.0.0.1:47334/item","port":47334,"external":"https://example.test:47334"}'
        )
    )
    proxy = Proxy(cassette)
    proxy.set_fixture_origin("http://127.0.0.1:39123")
    with proxy as base:
        response = _post(base, JEV, {"q": 1})
    assert response.json() == {
        "url": "http://127.0.0.1:39123/item",
        "port": 47334,
        "external": "https://example.test:47334",
    }


def test_translate_origin_requires_exact_origins() -> None:
    response = (
        "http://127.0.0.1:47334/x http://127.0.0.1:473340 http://127.0.0.1:4733/x 47334 "
        "https://example.test:47334 user@http://127.0.0.1:47334"
    )
    assert translate_origin(response, "http://127.0.0.1:47334", "http://127.0.0.1:39123") == (
        "http://127.0.0.1:39123/x http://127.0.0.1:473340 http://127.0.0.1:4733/x 47334 "
        "https://example.test:47334 user@http://127.0.0.1:47334"
    )


def test_translate_origin_handles_escaped_json_and_punctuation() -> None:
    response = r'"http://127.0.0.1:47334" (http://127.0.0.1:47334). http://127.0.0.1:47334, http://127.0.0.1:47334].'
    assert translate_origin(response, "http://127.0.0.1:47334", "http://127.0.0.1:39123") == (
        r'"http://127.0.0.1:39123" (http://127.0.0.1:39123). http://127.0.0.1:39123, http://127.0.0.1:39123].'
    )


def test_translate_origin_does_not_rewrite_authority_suffixes() -> None:
    response = "http://127.0.0.1:47334@evil.test http://127.0.0.1:47334."
    assert translate_origin(response, "http://127.0.0.1:47334", "http://127.0.0.1:39123") == (
        "http://127.0.0.1:47334@evil.test http://127.0.0.1:39123."
    )


def test_translate_response_origin_decodes_escaped_nested_json_strings() -> None:
    inner = json.dumps({"answer": "Sources:\nhttp://127.0.0.1:47334/a\thttp://127.0.0.1:47334/b"})
    response = json.dumps(
        {"choices": [{"message": {"content": inner}}], "external": "http://127.0.0.1:47334@evil.test"}
    )
    translated = translate_response_origin(response, "http://127.0.0.1:47334", "http://127.0.0.1:39123")
    assert json.loads(translated)["choices"][0]["message"]["content"] == json.dumps(
        {"answer": "Sources:\nhttp://127.0.0.1:39123/a\thttp://127.0.0.1:39123/b"},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def test_translate_response_origin_translates_non_json_body() -> None:
    response = "not json http://127.0.0.1:47334"
    assert (
        translate_response_origin(response, "http://127.0.0.1:47334", "http://127.0.0.1:39123")
        == "not json http://127.0.0.1:39123"
    )


def test_compare_runs_migrates_legacy_ports_and_all_result_fields() -> None:
    legacy = {
        "task": "t",
        "port": 47334,
        "result": {
            "protocol_error": "http://127.0.0.1:47334/error",
            "error": "http://127.0.0.1:47334/error",
            "citations": [{"url": "http://127.0.0.1:47334/c", "quote": "http://127.0.0.1:47334/q", "text": "x"}],
            "steps": [{"detail": "http://127.0.0.1:47334/detail"}],
            "grade": {"failure": "http://127.0.0.1:47334/grade"},
        },
    }
    current = {
        "task": "t",
        "fixture_origin": "http://127.0.0.1:51234",
        "result": {
            "protocol_error": "http://127.0.0.1:51234/error",
            "error": "http://127.0.0.1:51234/error",
            "citations": [{"url": "http://127.0.0.1:51234/c", "quote": "http://127.0.0.1:51234/q", "text": "x"}],
            "steps": [{"detail": "http://127.0.0.1:51234/detail"}],
            "grade": {"failure": "http://127.0.0.1:51234/grade"},
        },
    }
    assert compare_runs({"tasks": [legacy]}, {"tasks": [current]}) == {}


def test_replay_origin_roundtrip_preserves_response_contract() -> None:
    cassette = Cassette(task="t", meta={"port": 47334})
    cassette.exchanges.append(_exchange(JEV, {"q": 1}, '{"url":"http://127.0.0.1:47334/item","port":47334}'))
    proxy = Proxy(cassette)
    proxy.set_fixture_origin("http://127.0.0.1:39123")
    with proxy as base:
        response = _post(base, JEV, {"q": 1})
    assert response.status_code == 200
    assert response.headers["content-type"] == "application/json"
    assert response.json() == {"url": "http://127.0.0.1:39123/item", "port": 47334}
    assert cassette.meta == {"port": 47334}


def test_recording_saves_actual_origin_and_replays_at_two_new_ports(tmp_path: Path) -> None:
    cassette = Cassette(task="t", meta={"port": 47334})

    def forward(path: str, body: bytes) -> tuple[int, str, str]:
        return 201, "application/custom", '{"url":"http://127.0.0.1:39123/item","port":39123}'

    recording = Proxy(cassette, forward=forward, replay=False)
    recording.set_fixture_origin("http://127.0.0.1:39123")
    with recording as base:
        response = _post(base, JEV, {"q": 1})
    assert response.status_code == 201 and response.headers["content-type"] == "application/custom"
    cassette.save(tmp_path / "cassette.jsonl.gz")
    loaded = Cassette.load(tmp_path / "cassette.jsonl.gz")
    assert loaded.meta == {"port": 39123}

    for port in (40234, 41234):
        replay = Proxy(loaded)
        replay.set_fixture_origin(f"http://127.0.0.1:{port}")
        with replay as base:
            response = _post(base, JEV, {"q": 1})
        assert response.status_code == 201
        assert response.headers["content-type"] == "application/custom"
        assert response.json() == {"url": f"http://127.0.0.1:{port}/item", "port": 39123}
        assert loaded.meta == {"port": 39123}


def test_fill_records_live_origin_per_exchange_before_replay_at_another_port(tmp_path: Path) -> None:
    cassette = Cassette(task="t", meta={"port": 47334})
    cassette.exchanges.append(_exchange(JEV, {"q": "old"}, '{"url":"http://127.0.0.1:47334/old"}'))

    def forward(path: str, body: bytes) -> tuple[int, str, str]:
        return 200, "application/json", '{"url":"http://127.0.0.1:39123/new"}'

    filling = Proxy(cassette, forward=forward)
    filling.set_fixture_origin("http://127.0.0.1:39123")
    with filling as base:
        assert _post(base, JEV, {"q": "new"}).json()["url"].endswith(":39123/new")
    cassette.save(tmp_path / "filled.jsonl.gz")

    loaded = Cassette.load(tmp_path / "filled.jsonl.gz")
    assert loaded.exchanges[-1].fixture_origin == "http://127.0.0.1:39123"
    replay = Proxy(loaded)
    replay.set_fixture_origin("http://127.0.0.1:41234")
    with replay as base:
        response = _post(base, JEV, {"q": "new"})
    assert response.json() == {"url": "http://127.0.0.1:41234/new"}


def test_replay_miss_is_an_error_with_a_diff() -> None:
    cassette = Cassette(task="t")
    cassette.exchanges.append(_exchange(LLM, {"model": "m", "x": 1}, "{}"))
    proxy = Proxy(cassette)
    with proxy as base:
        response = _post(base, LLM, {"model": "m", "x": 2, "response_format": {"json_schema": {"name": "Plan"}}})
    assert response.status_code == MISS_STATUS
    assert "replay miss" in response.json()["error"]["message"]
    assert [m.purpose for m in proxy.misses] == ["llm:Plan"] and proxy.misses[0].model == "m"
    assert '+ "x": 2' in proxy.misses[0].diff


def test_record_forwards_everything_and_fill_only_misses() -> None:
    sent: list[str] = []

    def forward(path: str, body: bytes) -> tuple[int, str, str]:
        sent.append(json.loads(body)["q"])
        return 200, "application/json", json.dumps({"q": json.loads(body)["q"], "usage": {"cost": 0.01}})

    cassette = Cassette(task="t")
    with Proxy(cassette, forward=forward, replay=False) as base:
        _post(base, JEV, {"q": "a"})
        _post(base, JEV, {"q": "a"})
    assert sent == ["a", "a"] and len(cassette.exchanges) == 2
    assert [exchange.cost for exchange in cassette.exchanges] == [0.01, 0.01]

    filling = Proxy(cassette, forward=forward)
    with filling as base:
        assert _post(base, JEV, {"q": "a"}).json()["q"] == "a"
        assert _post(base, JEV, {"q": "b"}).json()["q"] == "b"
        assert _post(base, JEV, {"q": "b"}).json()["q"] == "b"
    assert sent == ["a", "a", "b"]
    assert filling.forwarded == 1 and [m.purpose for m in filling.misses] == ["jev:"]


def test_record_forwards_the_routing_a_run_sends_for_its_model(monkeypatch: pytest.MonkeyPatch) -> None:
    """A cassette holds answers from the endpoints a run is routed to, DeepSeek's pinned ones included."""
    sent: list[dict[str, object]] = []

    def upstream(request: httpx.Request) -> httpx.Response:
        sent.append(json.loads(request.content))
        return httpx.Response(200, json={})

    client = httpx.Client
    monkeypatch.setattr(httpx, "Client", lambda **kwargs: client(transport=httpx.MockTransport(upstream), **kwargs))
    forward = openrouter_forward("key")
    forward(LLM, json.dumps({"model": "deepseek/deepseek-v4.1-flash-1017"}).encode())
    forward(LLM, json.dumps({"model": "openai/gpt-6-luna"}).encode())
    forward(LLM, json.dumps({"model": "deepseek/deepseek-v4.1-flash-1017", "provider": {"only": ["x"]}}).encode())
    assert [body["provider"] for body in sent] == [
        {
            "require_parameters": True,
            "sort": "latency",
            "order": ["parasail", "deepinfra", "inference-net"],
            "ignore": ["together"],
        },
        {"require_parameters": True, "sort": "latency"},
        {"only": ["x"]},
    ]


def test_agent_gets_no_real_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "real")
    monkeypatch.setenv("AI_GATEWAY_API_KEY", "real")
    monkeypatch.setenv("FASTBROWSE_HEADED", "1")
    env = agent_environment("http://127.0.0.1:9")
    assert "real" not in env.values()
    assert "FASTBROWSE_HEADED" not in env and "AI_GATEWAY_API_KEY" not in env
    assert env["FASTBROWSE_DECISION_BASE_URL"] == "http://127.0.0.1:9/jev"
    assert env["FASTBROWSE_LLM_BASE_URL"] == "http://127.0.0.1:9/llm"


def test_corpus_task_ids_are_unique() -> None:
    tasks = corpus()
    assert len({t.id for t in tasks}) == len(tasks)


def test_normalized_results_ignore_only_the_live_fixture_origin() -> None:
    first = {"final_url": "http://127.0.0.1:41001/page", "answer": "port 41001", "other": "41001"}
    second = {"final_url": "http://127.0.0.1:42002/page", "answer": "port 42002", "other": "42002"}
    from fastbrowse.evals.bench import _canonicalize_fixture

    assert _canonicalize_fixture(first, "http://127.0.0.1:41001") == {
        "final_url": "<fixture-origin>/page",
        "answer": "port 41001",
        "other": "41001",
    }
    assert _canonicalize_fixture(second, "http://127.0.0.1:42002") == {
        "final_url": "<fixture-origin>/page",
        "answer": "port 42002",
        "other": "42002",
    }


def test_run_params_hand_over_chrome_and_scope_secrets() -> None:
    task = next(t for t in corpus() if t.id == "mock-sign-in")
    params = run_params(task, "http://127.0.0.1:47308", "ws://127.0.0.1:9/devtools/browser/x", "r")
    assert params["cdp_url"] == "ws://127.0.0.1:9/devtools/browser/x"
    assert params["secrets"] == [{"name": "password", "origins": ["http://127.0.0.1:47308"]}]
    assert "battery-staple" not in json.dumps(params)


def test_compare_reports_only_differing_tasks() -> None:
    def run(answer: str) -> dict[str, object]:
        return {"tasks": [{"task": "a", "result": {"answer": "same"}}, {"task": "b", "result": {"answer": answer}}]}

    assert compare_runs(run("x"), run("x")) == {}
    differences = compare_runs(run("x"), run("y"))
    assert list(differences) == ["b"]
    assert any(line.startswith('+ "answer": "y"') for line in differences["b"])


def test_chrome_target_ids_are_numbered_and_swapped_back() -> None:
    old_a, old_b = "B263C989BBEF4C3B6D03DD4B207B8F8C", "F0F04130F2C835E1D1DF50DFAFA6EF7C"
    new_a, new_b = "57B1BE83D83269F43C3E0D03C5D0FB26", "039D86F8FCB3567C10AD293C34379CFE"

    def state(a: str, b: str) -> dict[str, list[dict[str, str]]]:
        return {"tabs": [{"id": a, "title": "News"}, {"id": b, "opener_id": a, "title": "Story"}]}

    assert request_key(JEV, state(old_a, old_b)) == request_key(JEV, state(new_a, new_b))
    assert request_key(JEV, state(old_a, old_b)) != request_key(JEV, {"tabs": state(old_a, old_b)["tabs"][::-1]})
    # Keyed by id, a sorted map would put the tabs in the order of their random ids.
    criteria = {"criteria": {old_a: "News", old_b: "Story"}}
    flipped = {"criteria": {new_a: "News", new_b: "Story"}}  # new_a sorts after new_b, old_a before old_b
    assert request_key(JEV, criteria) == request_key(JEV, flipped)
    cassette = Cassette(task="t")
    cassette.exchanges.append(_exchange(JEV, state(old_a, old_b), json.dumps({"choice": old_b})))
    with Proxy(cassette) as base:
        assert _post(base, JEV, state(new_a, new_b)).json() == {"choice": new_b}


def test_note_ids_are_keyed_apart_from_the_digests_that_change_every_run() -> None:
    account, orders = "9c2068632cb0cf36db87072651e313e07cf9927d5edfd8fc862a55740229938e", "e3b0" * 16
    # The same notes on a fixture served from another port, with a control in a frame Chrome named anew: only each
    # id's address digest and the control's frame digest differ.
    old = {"account": "7d4ed89f79ff865f", "orders": "2db15fe124ef7eb5", "frame": "0a1a3e6c59ff288b"}
    new = {"account": "9b6003106a65a436", "orders": "f68234783bb37d0e", "frame": "78b05301b2a1baae"}

    def notes(run: dict[str, str]) -> dict[str, str]:
        return {
            "notes": f'- [{account}:0:12:{run["account"]}] quote="Your account"\n'
            f'- [{account}:13:21:{run["account"]}] quote="Password"\n'
            f'- [{orders}:0:6:{run["orders"]}] quote="Orders"\n'
            f'- [{orders}:7:13:{run["orders"]}:{run["frame"]}] quote="Cancel"'
        }

    assert request_key(LLM, notes(old)) == request_key(LLM, notes(new))
    # Notes from two pages are still told apart from notes that name one page twice.
    assert request_key(LLM, notes(old)) != request_key(LLM, notes(old | {"orders": old["account"]}))
    cassette = Cassette(task="t")
    cited = json.dumps({"cites": [f"{orders}:0:6:{old['orders']}", f"{orders}:7:13:{old['orders']}:{old['frame']}"]})
    cassette.exchanges.append(_exchange(LLM, notes(old), cited))
    with Proxy(cassette) as base:
        assert _post(base, LLM, notes(new)).json() == {
            "cites": [f"{orders}:0:6:{new['orders']}", f"{orders}:7:13:{new['orders']}:{new['frame']}"]
        }


def test_a_run_still_going_is_not_compared(tmp_path: Path) -> None:
    path = tmp_path / "run.json"
    path.write_text(json.dumps({"meta": {}, "tasks": [{"task": "a"}]}))
    with pytest.raises(SystemExit, match="still going"):
        load_run(path)
    path.write_text(json.dumps({"meta": {"finished": "2026-10-23T00:00:00+00:00"}, "tasks": []}))
    assert load_run(path)["tasks"] == []


def test_an_unpriced_exchange_is_counted_and_a_partial_run_cost_is_not_a_cost() -> None:
    exchange = Exchange("k", "/p", "plan", None, {}, 200, "application/json", "{}", 0.0, cost=0.25)
    assert priced([exchange, dataclasses.replace(exchange, cost=None)]) == (0.25, 1)
    metered = CostLine(component=CostComponent.LLM, dollars=0.25, basis=CostBasis.METERED)
    unknown = metered.model_copy(update={"dollars": None, "basis": CostBasis.UNKNOWN})
    assert run_dollars(CostBreakdown(lines=(metered, unknown))) is None
    assert run_dollars(CostBreakdown(lines=(metered,))) == 0.25


def _free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def _port(origin: str) -> int:
    return int(origin.rsplit(":", 1)[1])


def _suite_task(suite: str) -> BenchTask:
    return next(task for task in corpus() if task.suite == suite)


def test_fixture_port_is_the_newest_exchange_origin() -> None:
    a, b = 40001, 40002
    exchanges = [
        _exchange(JEV, {"q": 1}, "{}"),
        dataclasses.replace(_exchange(JEV, {"q": 2}, "{}"), fixture_origin=f"http://127.0.0.1:{a}"),
        dataclasses.replace(_exchange(JEV, {"q": 3}, "{}"), fixture_origin=f"http://127.0.0.1:{b}"),
        _exchange(JEV, {"q": 4}, "{}"),
    ]
    assert Cassette(task="t", exchanges=exchanges, meta={"port": 47312}).fixture_port() == b
    assert Cassette(task="t", exchanges=exchanges[:1], meta={"port": 47312}).fixture_port() == 47312
    assert Cassette(task="t", meta={"recorded_port": 47313, "port": 47312}).fixture_port() == 47313
    assert Cassette(task="t", exchanges=exchanges[:1]).fixture_port() is None


@pytest.mark.parametrize("suite", ["local", "mock"])
def test_fixture_site_binds_the_given_port(suite: str) -> None:
    port = _free_port()
    with fixture_site(_suite_task(suite), "http://127.0.0.1:9", port=port) as (base_url, _):
        assert _port(base_url) == port


@pytest.mark.parametrize("suite", ["local", "mock"])
def test_fixture_site_falls_back_and_warns_when_the_port_is_taken(
    suite: str, capsys: pytest.CaptureFixture[str]
) -> None:
    with socket.socket() as held:
        held.bind(("127.0.0.1", 0))
        held.listen()
        port = held.getsockname()[1]
        with fixture_site(_suite_task(suite), "http://127.0.0.1:9", port=port) as (base_url, _):
            assert _port(base_url) != port
    assert str(port) in capsys.readouterr().err


def test_a_bind_error_other_than_a_taken_port_is_raised() -> None:
    @contextmanager
    def refused(port: int):
        if port:
            raise OSError(errno.EACCES, "permission denied")
        yield "http://127.0.0.1:1", None

    with ExitStack() as stack, pytest.raises(PermissionError):
        _serve(_suite_task("local"), refused, _free_port(), stack)


def test_fixture_server_releases_its_port() -> None:
    """The next fill pass rebinds the same port at once, so the socket is closed on exit, not left to finalizers."""
    port = _free_port()
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always", ResourceWarning)
        for _ in range(2):
            with fixture_server(port) as (base_url, _):
                assert _port(base_url) == port
        gc.collect()
    assert not [w for w in caught if issubclass(w.category, ResourceWarning)]


def test_a_note_citing_request_hits_when_the_bench_binds_the_cassette_port() -> None:
    """A note id digests its page address, port included, which the key's port placeholder cannot reach."""

    def note(origin: str) -> dict[str, str]:
        address = hashlib.sha256(f"{origin}/shop.html".encode()).hexdigest()[:16]
        return {"note": f"[9c20:0:12:{address}] quote=Your account"}

    stale, recorded = _free_port(), _free_port()
    cassette = Cassette(task="t", meta={"port": stale})
    for origin in (f"http://127.0.0.1:{stale}", f"http://127.0.0.1:{recorded}"):
        exchange = _exchange(JEV, note(origin), '{"ok": true}')
        cassette.exchanges.append(dataclasses.replace(exchange, fixture_origin=origin))
    cassette.exchanges.pop(0)
    assert request_key(JEV, note(f"http://127.0.0.1:{_free_port()}")) != cassette.exchanges[0].key

    with fixture_site(_suite_task("local"), "http://127.0.0.1:9", port=cassette.fixture_port() or 0) as (base, _):
        proxy = Proxy(cassette)
        proxy.set_fixture_origin(base)
        with proxy as proxy_url:
            response = _post(proxy_url, JEV, note(base))
    assert response.status_code == 200 and proxy.misses == []
