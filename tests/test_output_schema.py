"""A JSON Schema turned into the model a run fills, as the caller of `output_model` sees it.

Each test hands over a schema and then asks the model what a run would: whether data validates, and what it
looks like dumped to JSON.
"""

import json
from typing import Any

import pytest
from pydantic import BaseModel, ValidationError

from fastbrowse.output_schema import UnsupportedSchema, output_model
from fastbrowse.page import BlockKind
from fastbrowse.verification import extract
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM, capture

ORDER: dict[str, Any] = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "id": {"type": "string"},
        "customer": {
            "type": "object",
            "properties": {"name": {"type": "string"}, "vip": {"type": "boolean"}},
            "required": ["name", "vip"],
        },
        "lines": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {"sku": {"type": "string"}, "quantity": {"type": "integer"}},
                "required": ["sku", "quantity"],
            },
        },
    },
    "required": ["id", "customer", "lines"],
}


def _dumped(model: type[BaseModel], data: Any) -> Any:
    """Data through the model and back to JSON, the way a run produces `RunResult.data`."""
    return model.model_validate(data).model_dump(mode="json")


def _accepts(schema: dict[str, Any], value: Any) -> bool:
    """Whether a property with this schema takes the value."""
    model = output_model({"type": "object", "properties": {"value": schema}, "required": ["value"]})
    try:
        model.model_validate({"value": value})
    except ValidationError:
        return False
    return True


def test_nested_objects_and_arrays_validate_matching_data() -> None:
    order = {
        "id": "A-17",
        "customer": {"name": "Ada", "vip": True},
        "lines": [{"sku": "kettle", "quantity": 2}, {"sku": "mug", "quantity": 6}],
    }

    assert _dumped(output_model(ORDER), order) == order


@pytest.mark.parametrize(
    "mismatched",
    [
        {"id": "A-17", "customer": {"name": "Ada"}, "lines": []},
        {"id": "A-17", "customer": {"name": "Ada", "vip": True}, "lines": [{"sku": "kettle", "quantity": "two"}]},
        {"id": "A-17", "customer": "Ada", "lines": []},
        {"id": "A-17", "customer": {"name": "Ada", "vip": True}, "lines": {"sku": "kettle", "quantity": 2}},
        {"customer": {"name": "Ada", "vip": True}, "lines": []},
    ],
    ids=["nested property missing", "wrong type in an array item", "object expected", "array expected", "no id"],
)
def test_nested_objects_and_arrays_reject_mismatched_data(mismatched: dict[str, Any]) -> None:
    with pytest.raises(ValidationError):
        output_model(ORDER).model_validate(mismatched)


def _refusal(schema: dict[str, Any]) -> UnsupportedSchema:
    with pytest.raises(UnsupportedSchema) as raised:
        output_model(schema)
    return raised.value


def _holding(schema: dict[str, Any]) -> dict[str, Any]:
    """An object schema with one required property, `value`, of the schema given."""
    return {"type": "object", "properties": {"value": schema}, "required": ["value"]}


def _case(case: object) -> str | None:
    return json.dumps(case) if isinstance(case, dict) else None


SKU = {"sku": {"type": "string"}}


@pytest.mark.parametrize(
    ("schema", "matching", "mismatched"),
    [
        ({"type": "string"}, ["kettle", ""], [3, None, ["kettle"]]),
        ({"type": "integer"}, [3, -1], ["three", "3", 3.5, True, None]),
        ({"type": "number"}, [3.5, 3], ["three", "3.5", True, None]),
        ({"type": "boolean"}, [True, False], ["yes", 1, None]),
        ({"type": "null"}, [None], ["null", 0]),
        ({"type": ["string", "null"]}, ["kettle", None], [3.5]),
        ({"type": "array", "items": {"type": "integer"}}, [[], [1, 2]], [["one"], 1, {"0": 1}]),
        ({"type": "array"}, [[], [1, "two", None]], [1, "one"]),
        ({"type": "object", "properties": SKU}, [{"sku": "kettle"}, {}], [{"sku": 3}, "kettle"]),
        ({"type": "object", "properties": SKU, "required": ["sku"]}, [{"sku": "kettle"}], [{}, {"sku": None}]),
        (
            {"type": "object", "properties": SKU, "additionalProperties": False},
            [{"sku": "kettle"}],
            [{"sku": "kettle", "price": 30}],
        ),
        (
            {"type": "object", "properties": SKU, "additionalProperties": True},
            [{"sku": "kettle", "price": 30}],
            [{"sku": 3}],
        ),
        ({"enum": ["small", "large", 3, None]}, ["small", 3, None], ["medium", 4]),
        ({"enum": [1.5, [1, 2], {"size": "small"}]}, [1.5, [1, 2], {"size": "small"}], [2.5, [2, 1], {}]),
        ({"type": "string", "enum": ["small", "large"]}, ["large"], ["medium", None]),
        ({"type": "integer", "enum": [1, "one"]}, [1], ["one", 2]),
        ({"const": "kettle"}, ["kettle"], ["mug", None]),
        ({"type": "integer", "const": 3}, [3], [4]),
        ({"const": 1}, [1], [True, "1"]),
        ({"enum": [True]}, [True], [1]),
        ({"anyOf": [{"type": "string"}, {"type": "null"}]}, ["kettle", None], [3.5]),
        ({"anyOf": [{"type": "null"}, {"type": "array", "items": {"type": "string"}}]}, [["a"], None], [[1], "a"]),
        ({"type": "string", "description": "What the item is called"}, ["kettle"], [3]),
        ({"type": "string", "title": "Name"}, ["kettle"], [3]),
        ({"type": "string", "default": "kettle"}, ["mug"], [3]),
        (
            {"type": "object", "properties": SKU, "additionalProperties": {}},
            [{"sku": "kettle", "price": 30}],
            [{"sku": 3}],
        ),
        ({"type": "integer", "minimum": 0}, [0, 7], [-1, 0.5, "0"]),
        ({"type": "integer", "maximum": 9007199254740991}, [9007199254740991], [9007199254740992]),
        ({"type": "number", "exclusiveMinimum": 0}, [0.5, 1], [0, -2]),
        ({"type": "number", "exclusiveMaximum": 1.5, "minimum": 1}, [1, 1.25], [1.5, 0.5]),
        ({"type": ["integer", "null"], "minimum": 0}, [3, None], [-3]),
        ({}, ["kettle", 3, None, [1], {"a": 1}], []),
    ],
    ids=_case,
)
def test_each_supported_keyword_is_enforced(schema: dict[str, Any], matching: list[Any], mismatched: list[Any]) -> None:
    assert [value for value in matching if not _accepts(schema, value)] == []
    assert [value for value in mismatched if _accepts(schema, value)] == []


def test_what_zod_writes_for_a_typical_schema_is_taken() -> None:
    # `z.object({...}).meta({title})` through `~standard.jsonSchema.input`, as Zod 4.6 writes it. `z.int()` is
    # where the bounds come from, `.default()` the default.
    model = output_model(
        {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The product name"},
                "count": {"type": "integer", "minimum": -9007199254740991, "maximum": 9007199254740991},
                "rating": {"type": ["number", "null"]},
                "size": {"type": "string", "enum": ["small", "large"]},
                "currency": {"default": "EUR", "type": "string"},
                "seller": {"$ref": "#/$defs/Seller"},
            },
            "required": ["name", "count", "rating", "size", "seller"],
            "title": "Product",
            "$defs": {"Seller": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        }
    )
    product = {"name": "Kettle", "count": 2, "rating": None, "size": "small", "seller": {"name": "Ada"}}

    assert _dumped(model, product) == product
    with pytest.raises(ValidationError):
        model.model_validate(product | {"count": 2**53})


def test_a_bound_is_not_part_of_the_schema_a_model_is_shown() -> None:
    model = output_model(_holding({"type": "integer", "minimum": 0}))

    assert model.model_json_schema()["properties"]["value"] == {"title": "Value", "type": "integer"}


def test_a_description_reaches_the_field_a_run_reads_it_from() -> None:
    model = output_model(_holding({"type": "string", "description": "The order number"}))

    assert model.model_fields["value"].description == "The order number"


def test_a_property_left_out_of_required_may_be_absent_and_comes_back_absent() -> None:
    model = output_model({"type": "object", "properties": {"sku": {"type": "string"}, "note": {"type": "string"}}})

    assert _dumped(model, {"sku": "kettle"}) == {"sku": "kettle"}


def test_a_null_the_schema_allows_comes_back_as_null() -> None:
    model = output_model({"type": "object", "properties": {"note": {"anyOf": [{"type": "string"}, {"type": "null"}]}}})

    assert _dumped(model, {"note": None}) == {"note": None}
    assert _dumped(model, {}) == {}


def test_definitions_are_followed_through_ref() -> None:
    model = output_model(
        {
            "type": "object",
            "properties": {
                "billing": {"$ref": "#/$defs/address"},
                "shipping": {"$ref": "#/$defs/address", "description": "Where it goes"},
                "history": {"type": "array", "items": {"$ref": "#/$defs/address"}},
            },
            "required": ["billing", "shipping", "history"],
            "$defs": {
                "address": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}, "country": {"$ref": "#/$defs/country"}},
                    "required": ["city", "country"],
                },
                "country": {"enum": ["nl", "de"]},
            },
        }
    )
    home = {"city": "Utrecht", "country": "nl"}
    order = {"billing": home, "shipping": home, "history": [home]}

    assert _dumped(model, order) == order
    with pytest.raises(ValidationError):
        model.model_validate(order | {"shipping": {"city": "Paris", "country": "fr"}})


@pytest.mark.parametrize(
    ("schema", "keyword", "at"),
    [
        ({"type": "string", "minLength": 1}, "minLength", "/minLength"),
        ({"type": "string", "pattern": "^A-"}, "pattern", "/pattern"),
        ({"type": "string", "format": "date"}, "format", "/format"),
        ({"type": "number", "multipleOf": 0.5}, "multipleOf", "/multipleOf"),
        ({"type": "string", "minimum": 0}, "minimum", "/minimum"),
        ({"minimum": 0}, "minimum", "/minimum"),
        ({"type": "integer", "minimum": "0"}, "minimum", "/minimum"),
        ({"type": "integer", "maximum": True}, "maximum", "/maximum"),
        ({"enum": [1, 2], "maximum": 1}, "maximum", "/maximum"),
        ({"type": "string", "title": 3}, "title", "/title"),
        ({"type": "array", "items": {"type": "string"}, "minItems": 1}, "minItems", "/minItems"),
        ({"type": "array", "items": {"type": "string", "maxLength": 3}}, "maxLength", "/items/maxLength"),
        ({"type": "array", "prefixItems": [{"type": "string"}]}, "prefixItems", "/prefixItems"),
        ({"type": "array", "uniqueItems": True}, "uniqueItems", "/uniqueItems"),
        ({"oneOf": [{"type": "string"}, {"type": "integer"}]}, "oneOf", "/oneOf"),
        ({"allOf": [{"type": "string"}]}, "allOf", "/allOf"),
        ({"not": {"type": "string"}}, "not", "/not"),
        ({"type": "object", "patternProperties": {"^x-": {}}}, "patternProperties", "/patternProperties"),
        (
            {"type": "object", "properties": {"a/b": {"type": "string", "maxLength": 3}}},
            "maxLength",
            "/properties/a~1b/maxLength",
        ),
        ({"anyOf": [{"type": "string"}, {"type": "integer"}]}, "anyOf", "/anyOf"),
        ({"anyOf": [{"type": "string", "minLength": 1}, {"type": "null"}]}, "minLength", "/anyOf/0/minLength"),
        ({"anyOf": [{"type": "string"}, {"type": "null"}], "type": "string"}, "type", "/type"),
        (
            {"type": "object", "additionalProperties": {"type": "string"}},
            "additionalProperties",
            "/additionalProperties",
        ),
        ({"type": "object", "required": ["sku"]}, "required", "/required"),
        ({"type": "string", "properties": SKU}, "properties", "/properties"),
        ({"type": "string", "items": {}}, "items", "/items"),
        ({"type": "date"}, "type", "/type"),
        ({"type": "string", "enum": [1, 2]}, "enum", "/enum"),
        ({"enum": []}, "enum", "/enum"),
        ({"enum": ["a"], "items": {}}, "items", "/items"),
        ({"$ref": "#/$defs/missing"}, "$ref", "/$ref"),
        ({"$ref": "https://example.com/address.json"}, "$ref", "/$ref"),
        ({"$ref": "#/properties/value"}, "$ref", "/$ref"),
        ({"$ref": "#"}, "$ref", "/$ref"),
        ({"$ref": "#/$defs/missing", "type": "string"}, "type", "/type"),
        ({"type": "object", "$defs": {"a": {"type": "string"}}}, "$defs", "/$defs"),
        ({"type": "array", "items": True}, "items", "/items"),
    ],
    ids=_case,
)
def test_a_keyword_that_cannot_be_honored_is_refused_by_name_and_path(
    schema: dict[str, Any], keyword: str, at: str
) -> None:
    refusal = _refusal(_holding(schema))
    path = f"#/properties/value{at}"

    assert (refusal.keyword, refusal.path) == (keyword, path)
    assert f'"{keyword}"' in str(refusal) and path in str(refusal)


@pytest.mark.parametrize(
    ("schema", "keyword", "path"),
    [
        ({"type": "array", "items": {"type": "string"}}, "type", "#/type"),
        ({"type": "string"}, "type", "#/type"),
        ({}, "type", "#/type"),
        ({"type": "object", "$schema": "http://json-schema.org/draft-07/schema#"}, "$schema", "#/$schema"),
        ({"type": "object", "$id": "https://example.com/order"}, "$id", "#/$id"),
        (
            {"type": "object", "$defs": {"unused": {"type": "string", "minLength": 1}}},
            "minLength",
            "#/$defs/unused/minLength",
        ),
    ],
    ids=_case,
)
def test_a_root_that_cannot_be_a_runs_output_is_refused(schema: dict[str, Any], keyword: str, path: str) -> None:
    refusal = _refusal(schema)

    assert (refusal.keyword, refusal.path) == (keyword, path)


@pytest.mark.parametrize(
    ("definitions", "path"),
    [
        (
            {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            "#/$defs/node/properties/next/$ref",
        ),
        (
            {
                "node": {"type": "object", "properties": {"children": {"$ref": "#/$defs/nodes"}}},
                "nodes": {"type": "array", "items": {"$ref": "#/$defs/node"}},
            },
            "#/$defs/nodes/items/$ref",
        ),
        ({"node": {"anyOf": [{"$ref": "#/$defs/node"}, {"type": "null"}]}}, "#/$defs/node/anyOf/0/$ref"),
    ],
    ids=["itself", "through another definition", "through anyOf"],
)
def test_a_recursive_ref_is_refused(definitions: dict[str, Any], path: str) -> None:
    refusal = _refusal({"type": "object", "properties": {"tree": {"$ref": "#/$defs/node"}}, "$defs": definitions})

    assert (refusal.keyword, refusal.path) == ("$ref", path)
    assert "recursive" in str(refusal)


AWKWARD = ["order-id", "2fa", "class", "copy", "model_dump", "_private", "", "total price", "naïve", "a/b", "~"]


@pytest.mark.parametrize("name", AWKWARD)
def test_a_property_python_could_not_name_round_trips_under_the_callers_name(name: str) -> None:
    model = output_model({"type": "object", "properties": {name: {"type": "string"}}, "required": [name]})

    assert _dumped(model, {name: "kept"}) == {name: "kept"}


def test_awkward_names_round_trip_together_and_at_every_depth() -> None:
    names = {name: {"type": "string"} for name in AWKWARD}
    inner = {"type": "object", "properties": names, "required": list(names)}
    # `order_id` is what `order-id` would otherwise be called, and both have to survive.
    properties = names | {"order_id": {"type": "string"}, "in": inner, "for": {"type": "array", "items": inner}}
    model = output_model({"type": "object", "properties": properties, "required": list(properties)})
    flat = {name: f"value of {name}" for name in AWKWARD}
    data = flat | {"order_id": "underscored", "in": flat, "for": [flat, flat]}

    assert _dumped(model, data) == data


def test_a_run_that_fills_the_model_by_field_name_gets_the_callers_names_back() -> None:
    model = output_model(
        {
            "type": "object",
            "properties": {"order-id": {"type": "string"}, "class": {"type": "integer"}, "total": {"type": "number"}},
            "required": ["order-id", "class", "total"],
        }
    )
    fields = {field.alias or name: name for name, field in model.model_fields.items()}

    # `verification.extract` keys what it found by field name and dumps with no arguments.
    filled = {fields["order-id"]: "A-17", fields["class"]: 2, fields["total"]: 9.5}

    assert _dumped(model, filled) == {"order-id": "A-17", "class": 2, "total": 9.5}


async def test_extraction_returns_a_scalar_under_the_callers_name() -> None:
    model = output_model(
        {
            "type": "object",
            "properties": {"unit-count": {"type": "integer", "description": "How many units are in stock"}},
            "required": ["unit-count"],
        }
    )
    page = capture((BlockKind.PARAGRAPH, "In stock: 3 units."))

    extraction = await extract(ScriptedJev({}), ScriptedLLM([]), "How many units are in stock?", page, model)

    assert extraction.problem is None
    assert extraction.data == {"unit-count": 3}
