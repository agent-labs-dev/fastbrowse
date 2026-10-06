"""`output_schema` in a `run`, as a client sees it, with `run_task` replaced by a stand-in.

The schemas themselves are covered in `test_output_schema.py`. Here it is the trip that is asserted: a JSON
Schema goes in, `run_task` gets a model, and what that model produced comes back under the caller's names.
"""

from typing import Any

import pytest
from pydantic import BaseModel, ValidationError

from fastbrowse.models import RunResult
from tests.serve_client import serving
from tests.test_serve_run import Recorder, _result, _run, _settings

ORDER: dict[str, Any] = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "order-id": {"type": "string", "description": "The order number on the confirmation page"},
        "class": {"type": "string"},
        "lines": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {"sku": {"type": "string"}, "2-day": {"type": "boolean"}},
                "required": ["sku", "2-day"],
                "additionalProperties": False,
            },
        },
    },
    "required": ["order-id", "class", "lines"],
    "additionalProperties": False,
}


class Filling:
    """Stands where `run_task` does and fills the model as a run does: by field name, dumped with no arguments."""

    def __init__(self, found: dict[str, Any]) -> None:
        self._found = found

    async def __call__(self, task: str, *, output_schema: type[BaseModel], **arguments: Any) -> RunResult:
        names = {field.alias or name: name for name, field in output_schema.model_fields.items()}
        values = {names[key]: value for key, value in self._found.items()}
        return _result(data=output_schema.model_validate(values).model_dump(mode="json"))


async def test_a_run_without_an_output_schema_hands_run_task_none() -> None:
    recorder = Recorder()

    async with serving(runner=recorder, settings=_settings) as client:
        await client.result("run", _run())

    assert recorder.calls[0]["output_schema"] is None


async def test_a_nested_schema_reaches_run_task_as_a_model() -> None:
    recorder = Recorder()

    async with serving(runner=recorder, settings=_settings) as client:
        await client.result("run", _run(output_schema=ORDER))

    model = recorder.calls[0]["output_schema"]
    order = {"order-id": "A-17", "class": "express", "lines": [{"sku": "kettle", "2-day": True}]}
    assert model.model_validate(order).model_dump(mode="json") == order
    assert {field.description for field in model.model_fields.values()} >= {"The order number on the confirmation page"}
    with pytest.raises(ValidationError):
        model.model_validate(order | {"lines": [{"sku": "kettle"}]})
    with pytest.raises(ValidationError):
        model.model_validate(order | {"lines": "kettle"})


async def test_the_result_carries_the_data_under_the_callers_property_names() -> None:
    order = {"order-id": "A-17", "class": "express", "lines": [{"sku": "kettle", "2-day": True}]}

    async with serving(runner=Filling(order), settings=_settings) as client:
        result = await client.result("run", _run(output_schema=ORDER))

    assert result["data"] == order


@pytest.mark.parametrize(
    ("schema", "keyword", "path"),
    [
        (
            {"type": "object", "properties": {"id": {"type": "string", "pattern": "^A-"}}},
            "pattern",
            "#/properties/id/pattern",
        ),
        (
            {
                "type": "object",
                "properties": {"tree": {"$ref": "#/$defs/node"}},
                "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            },
            "$ref",
            "#/$defs/node/properties/next/$ref",
        ),
        ({"type": "array", "items": {"type": "string"}}, "type", "#/type"),
    ],
    ids=["unsupported keyword", "recursive $ref", "root is not an object"],
)
async def test_a_schema_the_server_cannot_honor_is_refused_before_a_run_starts(
    schema: dict[str, Any], keyword: str, path: str
) -> None:
    recorder = Recorder()

    async with serving(runner=recorder, settings=_settings) as client:
        response = await client.request("run", _run(output_schema=schema))
        after = await client.result("run", _run("run-2"))

    assert response["error"]["code"] == -32004
    assert f'"{keyword}"' in response["error"]["message"] and path in response["error"]["message"]
    # Nothing ran for the refused request, and the server is free for the next one.
    assert [call["task"] for call in recorder.calls] == ["Add the kettle to the cart"]
    assert after["status"] == "complete"


async def test_an_output_schema_that_is_not_an_object_is_invalid_params() -> None:
    recorder = Recorder()

    async with serving(runner=recorder, settings=_settings) as client:
        response = await client.request("run", _run(output_schema="order"))

    assert response["error"]["code"] == -32602
    assert "output_schema" in response["error"]["message"]
    assert recorder.calls == []
