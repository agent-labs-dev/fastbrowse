"""Print the JSON Schema the JavaScript SDK's types are generated from.

Every model `fastbrowse.protocol` defines is in it, found by looking at the module, so a message added there
reaches the SDK without being listed here. `RunResult` is added by name, since it is the reply to `run` and
the protocol module only imports it.

Beside the models, under `constants`, are the values a client has to hold the same as the server: the protocol
version, and the members of each enum the module defines, which are the error codes and the method names.

    uv run python scripts/sdk_schema.py

`npm run generate:sdk` runs this and writes `packages/sdk/src/protocol.ts` from what it prints.
"""

import json
import sys
from enum import Enum
from types import ModuleType
from typing import Any

from pydantic import BaseModel
from pydantic.json_schema import GenerateJsonSchema, JsonSchemaMode, models_json_schema
from pydantic_core import core_schema

from fastbrowse import protocol
from fastbrowse.models import RunResult


class _AsWritten(GenerateJsonSchema):
    """A model the server writes has every field in its JSON, a default included.

    Pydantic calls a field with a default optional in both directions. Left that way, the SDK's types would make
    a caller check for a missing `citations` that is always there.
    """

    def field_is_required(
        self,
        field: core_schema.ModelField | core_schema.DataclassField | core_schema.TypedDictField,
        total: bool,
    ) -> bool:
        if self.mode == "serialization":
            return not field.get("serialization_exclude")
        return super().field_is_required(field, total)


def _mode(model: type[BaseModel]) -> JsonSchemaMode:
    """Which direction a model travels. `Params` is what the server reads, and it writes everything else."""
    return "validation" if issubclass(model, getattr(protocol, "Params", ())) else "serialization"


def _models(module: ModuleType) -> list[type[BaseModel]]:
    defined = [
        value
        for value in vars(module).values()
        if isinstance(value, type) and issubclass(value, BaseModel) and value.__module__ == module.__name__
    ]
    return sorted([*defined, RunResult], key=lambda model: model.__name__)


def _for_the_type_generator(node: Any, *, named: bool = False) -> Any:
    """The same schema in the dialect json-schema-to-typescript reads.

    It names a type after any `title` it finds, and pydantic titles every field, which would turn `run_id` into
    a `RunId` alias of `string`. It also predates `prefixItems`, and reads a tuple only from a list in `items`.
    """
    if isinstance(node, list):
        return [_for_the_type_generator(item) for item in node]
    if not isinstance(node, dict):
        return node
    return {
        ("items" if key == "prefixItems" else key): (
            # The values of `$defs` and `properties` are schemas keyed by name, and a name may be "title".
            {name: _for_the_type_generator(schema, named=key == "$defs") for name, schema in value.items()}
            if key in ("$defs", "properties")
            else _for_the_type_generator(value)
        )
        for key, value in node.items()
        if key != "title" or named
    }


def _constants(module: ModuleType) -> dict[str, Any]:
    """The module's enums by member name, and its upper-case numbers, such as `PROTOCOL_VERSION`."""
    found: dict[str, Any] = {}
    for name, value in vars(module).items():
        if isinstance(value, type) and issubclass(value, Enum) and value.__module__ == module.__name__:
            found[name] = {member.name: member.value for member in value}
        elif name.isupper() and isinstance(value, int):
            found[name] = value
    return dict(sorted(found.items()))


def schema(module: ModuleType = protocol) -> dict[str, Any]:
    """One document whose `$defs` hold every model in `module`, `RunResult`, and whatever those refer to.

    Its `constants` are no part of JSON Schema. The type generator takes them out before it reads the rest.
    """
    _, document = models_json_schema([(model, _mode(model)) for model in _models(module)], schema_generator=_AsWritten)
    return _for_the_type_generator(document) | {"constants": _constants(module)}


if __name__ == "__main__":
    json.dump(schema(), sys.stdout, indent=2)
    sys.stdout.write("\n")
