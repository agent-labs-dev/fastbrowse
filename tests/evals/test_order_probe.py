import json
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.evals import order_probe
from fastbrowse.evals.order_probe import positions, probe, report, requests, reversed_request
from fastbrowse.evals.replay import JEV_MOUNT, LLM_MOUNT, Cassette, Exchange


def choice(*options: str) -> dict[str, Any]:
    return {
        "type": "choice",
        "instructions": "Pick one.",
        "criteria": {option: f"means {option}" for option in options},
    }


def noul() -> dict[str, Any]:
    return {"type": "noul", "instructions": "True?", "criteria": {"true": "yes", "false": "no"}}


def body(**questions: dict[str, Any]) -> dict[str, Any]:
    return {"model": "jev-1.14", "state": {"text": "page"}, "questions": questions, "assets": [{"id": "a"}]}


def answered(request: dict[str, Any], probabilities: dict[str, dict[str, float]], cost: float = 0.01) -> dict[str, Any]:
    answers: dict[str, Any] = {}
    for name, question in request["questions"].items():
        if question["type"] != "choice":
            answers[name] = {"type": "noul", "noul": 0.5}
            continue
        odds = probabilities[name]
        answers[name] = {"type": "choice", "choice": max(odds, key=lambda option: odds[option]), "probabilities": odds}
    return {"answers": answers, "usage": {"cost": cost}}


def fake_jev(lean: bool, cost: float = 0.01):
    """A Jev that puts 0.7 on whichever option is listed first, or that answers from the option key alone."""
    sent: list[dict[str, Any]] = []

    def forward(path: str, raw: bytes) -> tuple[int, str, str]:
        request = json.loads(raw)
        sent.append(request)
        odds: dict[str, dict[str, float]] = {}
        for name, question in request["questions"].items():
            options = list(question["criteria"])
            if lean:
                odds[name] = {option: (0.7 if i == 0 else 0.3 / (len(options) - 1)) for i, option in enumerate(options)}
            else:
                odds[name] = {option: (0.6 if option == "undone" else 0.4) for option in options}
        return 200, "application/json", json.dumps(answered(request, odds, cost))

    return forward, sent


def exchange(path: str, request: dict[str, Any], response: dict[str, Any], cost: float | None = None) -> Exchange:
    return Exchange(
        key="",
        path=path,
        purpose="jev",
        model="jev-1.14",
        request=request,
        status=200,
        content_type="application/json",
        body=json.dumps(response),
        latency=0.1,
        cost=cost,
    )


def test_reversed_request_reverses_every_choice_and_nothing_else() -> None:
    original = body(actions_done=choice("done", "undone"), step=choice("click", "fill", "read"), seen=noul())
    flipped = reversed_request(original)
    assert list(flipped["questions"]["actions_done"]["criteria"]) == ["undone", "done"]
    assert list(flipped["questions"]["step"]["criteria"]) == ["read", "fill", "click"]
    assert flipped["questions"]["step"]["criteria"]["fill"] == "means fill"
    assert flipped["questions"]["seen"] == original["questions"]["seen"]
    assert flipped["state"] == original["state"] and flipped["assets"] == original["assets"]
    assert list(original["questions"]["actions_done"]["criteria"]) == ["done", "undone"]


def test_requests_reads_jev_exchanges_from_cassettes_and_http_events_from_traces(tmp_path: Path) -> None:
    gate = body(actions_done=choice("done", "undone"))
    recorded = answered(gate, {"actions_done": {"done": 0.8, "undone": 0.2}})
    Cassette(
        task="t",
        exchanges=[
            exchange(f"{JEV_MOUNT}/v1/systemone", gate, recorded),
            exchange(f"{JEV_MOUNT}/v1/systemone", body(seen=noul()), {"answers": {}}),
            exchange(f"{LLM_MOUNT}/chat/completions", body(actions_done=choice("done", "undone")), {"choices": []}),
        ],
    ).save(tmp_path / "t.jsonl.gz")
    trace = tmp_path / "trace.jsonl"
    confirm = body(stops_to_confirm=choice("confirm", "no"))
    trace.write_text(
        json.dumps({"event": "step", "n": 1})
        + "\n"
        + json.dumps(
            {
                "event": "http",
                "url": "https://openrouter.ai/api/v1/systemone",
                "request": confirm,
                "response": answered(confirm, {"stops_to_confirm": {"confirm": 0.4, "no": 0.6}}),
            }
        )
        + "\n"
    )
    found = list(requests([tmp_path / "t.jsonl.gz", trace]))
    assert [request for _, request, _ in found] == [gate, confirm]
    assert found[0][2] == recorded
    assert found[0][0].endswith("t.jsonl.gz") and found[1][0].endswith("trace.jsonl")


def test_positions_gives_each_option_its_offered_place_and_recorded_probability() -> None:
    request = body(target=choice("c0", "c1", "c12"), seen=noul())
    rows = positions(request, answered(request, {"target": {"c0": 0.2, "c1": 0.5, "c12": 0.3}}))
    assert len(rows) == 1
    row = rows[0]
    assert row.question == "target" and row.options == ["c0", "c1", "c12"]
    assert row.probabilities == {"c0": 0.2, "c1": 0.5, "c12": 0.3}
    assert row.group == "cN"
    assert positions(body(gate=choice("main:3", "main:7")), None)[0].group == "main:N"


def recorded_gates() -> list[tuple[str, dict[str, Any], dict[str, Any] | None]]:
    items: list[tuple[str, dict[str, Any], dict[str, Any] | None]] = []
    for i in range(3):
        request = body(actions_done=choice("done", "undone"))
        items.append((f"s{i}", request, answered(request, {"actions_done": {"done": 0.7, "undone": 0.3}})))
    return items


def test_a_first_option_lean_shows_as_a_shift_and_a_flip_on_reversal() -> None:
    forward, _ = fake_jev(lean=True)
    trials, _ = probe(recorded_gates(), forward)
    (group,) = report(trials)
    assert group["group"] == "done/undone" and group["n"] == 3
    assert group["shift"] == pytest.approx(0.4)
    assert group["reversal_flips"] == pytest.approx(1.0)
    assert group["noise_flips"] == pytest.approx(0.0)


def test_an_answer_from_the_option_alone_shows_no_shift() -> None:
    forward, _ = fake_jev(lean=False)
    items = [
        (s, request, answered(request, {"actions_done": {"done": 0.4, "undone": 0.6}}))
        for s, request, _ in recorded_gates()
    ]
    (group,) = report(probe(items, forward)[0])
    assert group["shift"] == pytest.approx(0.0)
    assert group["reversal_flips"] == pytest.approx(0.0)
    assert group["noise_flips"] == pytest.approx(0.0)


def test_the_budget_stops_before_the_total_would_pass_the_cap() -> None:
    forward, sent = fake_jev(lean=True, cost=0.01)
    trials, spent = probe(recorded_gates(), forward, max_dollars=0.05)
    # Each request is sent twice at 0.01 a send: a third pair would bring the total to 0.06.
    assert len(trials) == 2 and len(sent) == 4
    assert spent == pytest.approx(0.04)


def test_with_a_cap_a_request_with_no_cost_to_go_on_is_not_sent() -> None:
    forward, sent = fake_jev(lean=True, cost=0.04)
    unpriced = [(s, request, {"answers": {}}) for s, request, _ in recorded_gates()]
    trials, spent = probe(unpriced, forward, max_dollars=0.0)
    assert trials == [] and sent == [] and spent == 0.0


def test_only_a_blanked_image_asset_keeps_a_request_from_being_resent() -> None:
    forward, sent = fake_jev(lean=True)
    literal = body(actions_done=choice("done", "undone"))
    literal["state"] = {"text": "a page that says <image>"}
    blanked = body(actions_done=choice("done", "undone"))
    blanked["assets"] = {"screenshot-a": {"type": "image", "data_base64": "<image>"}}
    recorded = [
        (s, request, answered(request, {"actions_done": {"done": 0.7, "undone": 0.3}}))
        for s, request in [("literal", literal), ("blanked", blanked)]
    ]
    trials, _ = probe(recorded, forward)
    assert [trial.source for trial in trials] == ["literal"] and len(sent) == 2


def test_paid_mode_needs_both_the_spend_flag_and_a_key(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
    assert order_probe.main(["--out", str(tmp_path), "--max-dollars", "1"]) != 0
    assert order_probe.main(["--out", str(tmp_path), "--yes-spend", "--max-dollars", "1"]) != 0
    monkeypatch.setenv("OPENROUTER_API_KEY", "key")
    assert order_probe.main(["--out", str(tmp_path), "--yes-spend"]) != 0
    assert order_probe.main(["--out", str(tmp_path), "--yes-spend", "--max-dollars", "nan"]) != 0
