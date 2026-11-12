import gzip
import json
from pathlib import Path

from scripts.jev_state_size import characters, profile


def test_profiles_count_unanswered_states_without_inventing_tokens(tmp_path: Path) -> None:
    state = {"page": "A\n雪", "controls": []}
    request = {"state": state, "questions": {"carried": {"type": "noul"}}}
    trace = tmp_path / "trace.jsonl"
    trace.write_text(
        "\n".join(
            json.dumps(row)
            for row in [
                {"event": "http", "request": request, "response": {"usage": {"inputTokens": 50}}},
                {"event": "http", "request": request, "outcome": "cancelled"},
                {"event": "step", "request": request},
            ]
        )
    )
    cassette = tmp_path / "cassette.jsonl.gz"
    with gzip.open(cassette, "wt") as output:
        output.write(json.dumps({"request": request, "body": '{"usage":{"input_tokens":60}}'}))
    measured = profile([trace, cassette])["kinds"]["carried"]
    assert measured["requests"] == 3
    assert measured["metered_requests"] == 2
    assert measured["input_tokens"] == 110
    assert measured["state_chars"] == 3 * len('{"page":"A\\n雪","controls":[]}')
    assert measured["fields"] == {"page": 3 * characters("A\n雪"), "controls": 6}
