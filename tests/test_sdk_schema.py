"""The JSON Schema the JavaScript SDK's types are generated from, as `scripts/sdk_schema.py` writes it."""

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

from pydantic import BaseModel

from fastbrowse import protocol

_SCRIPT = Path(__file__).parents[1] / "scripts" / "sdk_schema.py"
_spec = importlib.util.spec_from_file_location("sdk_schema", _SCRIPT)
assert _spec is not None and _spec.loader is not None
sdk_schema = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sdk_schema)


def _definitions() -> dict[str, Any]:
    return sdk_schema.schema(protocol)["$defs"]


def test_every_model_the_protocol_module_defines_is_in_the_schema() -> None:
    defined = {
        name
        for name, value in vars(protocol).items()
        if isinstance(value, type) and issubclass(value, BaseModel) and value.__module__ == protocol.__name__
    }

    assert {"RunParams", "RunEvent", "InitializeResult", "ErrorResponse"} <= defined
    assert defined <= _definitions().keys()


def test_the_result_and_the_events_are_in_the_schema_with_the_models_they_hold() -> None:
    assert {"RunResult", "StepEvent", "BrowserEvent", "StepResult", "CostBreakdown", "Status"} <= _definitions().keys()


def test_a_model_added_to_the_module_appears_without_being_listed_anywhere() -> None:
    class Ping(BaseModel):
        sent_at: float

    module = ModuleType("later_protocol")
    Ping.__module__ = module.__name__
    module.Ping = Ping  # ty: ignore[unresolved-attribute]

    assert sdk_schema.schema(module)["$defs"]["Ping"]["properties"].keys() == {"sent_at"}


def test_the_result_is_described_as_it_is_written_and_not_as_it_is_read() -> None:
    result = _definitions()["RunResult"]

    # `final_frame` is excluded from every JSON result, so a type that listed it would promise a field nobody gets.
    assert "final_frame" not in result["properties"]
    # The server writes every field, defaults included, so the client never has to handle a missing one.
    assert set(result["required"]) == result["properties"].keys()


def test_a_step_frame_is_base64_text_or_null() -> None:
    frame = _definitions()["StepEvent"]["properties"]["frame"]

    # The frame's serializer leaves None alone, and a run over the protocol has no frame at all.
    assert frame["anyOf"] == [{"type": "string"}, {"type": "null"}]


def test_what_the_client_sends_keeps_its_defaults_optional() -> None:
    assert set(_definitions()["RunParams"]["required"]) == {"run_id", "task"}


def test_a_fixed_length_tuple_is_written_in_the_form_the_type_generator_reads() -> None:
    viewport = next(
        option for option in _definitions()["RunParams"]["properties"]["viewport"]["anyOf"] if "items" in option
    )

    assert viewport["items"] == [{"type": "integer"}, {"type": "integer"}]
    assert (viewport["minItems"], viewport["maxItems"]) == (2, 2)
    assert "prefixItems" not in json.dumps(sdk_schema.schema(protocol))


def test_the_script_prints_the_same_schema_every_time() -> None:
    def printed() -> str:
        return subprocess.run([sys.executable, str(_SCRIPT)], capture_output=True, text=True, check=True).stdout

    first = printed()

    assert json.loads(first) == sdk_schema.schema(protocol)
    assert first == printed()
