import json
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals.bench import agent_environment, compare_runs, corpus, run_params
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
    request_key,
)
from fastbrowse.models import Operation
from fastbrowse.page import Control

LLM = "/llm/chat/completions"
JEV = "/jev/v1/systemone"


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
    def state(day: str, weekday: str, months: tuple[str, str], week: list[str]) -> dict[str, object]:
        return {
            "date": f"{day} ({weekday}). In one month: {months[0]}; in two months: {months[1]}",
            "next_days": week,
            "captured_at": f"{day}T10:11:12.345678+00:00",
        }

    friday = state("2026-10-23", "Friday", ("November 2026", "December 2026"), ["Saturday 2026-10-24"])
    monday = state("2027-02-01", "Monday", ("March 2027", "April 2027"), ["Tuesday 2027-02-02"])
    assert request_key(LLM, friday) == request_key(LLM, monday)
    assert canonical_value(friday) == {"date": "<today>", "next_days": ["<next day>"], "captured_at": "<timestamp>"}


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


def test_control_operations_are_written_sorted() -> None:
    operations = frozenset({Operation.FILL, Operation.CLICK, Operation.ENTER, Operation.SELECT})
    control = Control(id="1", frame_id=None, role="combobox", label="Size", operations=operations)
    assert control.model_dump(mode="json")["operations"] == ["click", "enter", "fill", "select"]
    assert Control.model_validate_json(control.model_dump_json()).operations == operations


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


def test_miss_diff_is_cut() -> None:
    diff = miss_diff({"a": list(range(500))}, {"a": []}, limit=10)
    assert len(diff.splitlines()) == 11
    assert diff.endswith("more lines")


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


def test_replay_miss_is_an_error_with_a_diff() -> None:
    cassette = Cassette(task="t")
    cassette.exchanges.append(_exchange(LLM, {"model": "m", "x": 1}, "{}"))
    misses = []
    proxy = Proxy(cassette, on_miss=misses.append)
    with proxy as base:
        response = _post(base, LLM, {"model": "m", "x": 2, "response_format": {"json_schema": {"name": "Plan"}}})
    assert response.status_code == MISS_STATUS
    assert "replay miss" in response.json()["error"]["message"]
    assert [m.purpose for m in misses] == ["llm:Plan"] and misses[0].model == "m"
    assert '+ "x": 2' in misses[0].diff
    assert proxy.misses == misses


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
    assert cassette.cost == pytest.approx(0.02)

    filling = Proxy(cassette, forward=forward)
    with filling as base:
        assert _post(base, JEV, {"q": "a"}).json()["q"] == "a"
        assert _post(base, JEV, {"q": "b"}).json()["q"] == "b"
        assert _post(base, JEV, {"q": "b"}).json()["q"] == "b"
    assert sent == ["a", "a", "b"]
    assert filling.forwarded == 1 and [m.purpose for m in filling.misses] == ["jev:"]


def test_agent_gets_no_real_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "real")
    monkeypatch.setenv("AI_GATEWAY_API_KEY", "real")
    monkeypatch.setenv("FASTBROWSE_LLM_MODEL_PLAN", "other")
    env = agent_environment("http://127.0.0.1:9")
    assert "real" not in env.values()
    assert "FASTBROWSE_LLM_MODEL_PLAN" not in env and "AI_GATEWAY_API_KEY" not in env
    assert env["FASTBROWSE_JEV_BASE_URL"] == "http://127.0.0.1:9/jev"
    assert env["FASTBROWSE_LLM_BASE_URL"] == "http://127.0.0.1:9/llm"


def test_every_task_has_its_own_fixed_port() -> None:
    tasks = corpus(47300)
    assert len({t.port for t in tasks}) == len(tasks) == len({t.id for t in tasks})
    assert [t.port for t in tasks] == [t.port for t in corpus(47300)]


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
