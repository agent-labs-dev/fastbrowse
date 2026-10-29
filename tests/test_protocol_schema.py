"""`protocol/schema.json`, which the Rust core writes and every client's types are generated from, is what pydantic
writes for the Python protocol models. Both cores speak one protocol until the Python one is gone."""

import json
from enum import Enum
from pathlib import Path
from typing import Any

from pydantic import BaseModel
from pydantic.json_schema import GenerateJsonSchema, JsonSchemaMode, models_json_schema
from pydantic_core import core_schema

from fastbrowse import protocol
from fastbrowse.models import RunResult

_SCHEMA = Path(__file__).parents[1] / "protocol" / "schema.json"


class _AsWritten(GenerateJsonSchema):
    """A model the server writes has every field in its JSON, a default included."""

    def field_is_required(
        self,
        field: core_schema.ModelField | core_schema.DataclassField | core_schema.TypedDictField,
        total: bool,
    ) -> bool:
        if self.mode == "serialization":
            return not field.get("serialization_exclude")
        return super().field_is_required(field, total)


def _mode(model: type[BaseModel]) -> JsonSchemaMode:
    """`Params` is what the server reads, and it writes everything else."""
    return "validation" if issubclass(model, protocol.Params) else "serialization"


def _pydantic_document() -> dict[str, Any]:
    models = [
        value
        for value in vars(protocol).values()
        if isinstance(value, type) and issubclass(value, BaseModel) and value.__module__ == protocol.__name__
    ]
    _, document = models_json_schema(
        [(model, _mode(model)) for model in sorted([*models, RunResult], key=lambda model: model.__name__)],
        schema_generator=_AsWritten,
    )
    constants: dict[str, Any] = {}
    for name, value in vars(protocol).items():
        if isinstance(value, type) and issubclass(value, Enum) and value.__module__ == protocol.__name__:
            constants[name] = {member.name: member.value for member in value}
        elif name.isupper() and isinstance(value, int):
            constants[name] = value
    return document | {"constants": dict(sorted(constants.items()))}


def test_the_rust_schema_is_the_one_pydantic_writes_for_the_python_models() -> None:
    assert json.loads(_SCHEMA.read_text()) == _pydantic_document()
