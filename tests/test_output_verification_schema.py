import pytest
from pydantic import ValidationError

from fastbrowse.clients.openai_compatible import strict_schema
from fastbrowse.verification import _assessment_schema, _identity_schema


@pytest.mark.parametrize("key", ["criterion_17", "arbitrary-field"])
def test_identity_response_requires_each_requested_key(key):
    schema = _identity_schema((key, "other"))
    with pytest.raises(ValidationError):
        schema.model_validate({"identities": [], "bindings": {}})
    with pytest.raises(ValidationError):
        schema.model_validate({"identities": [], "bindings": {key: {"scope": "unresolved"}}})
    response = schema.model_validate(
        {"identities": [], "bindings": {key: {"scope": "unresolved"}, "other": {"scope": "subjectless"}}}
    )
    assert set(response.model_dump()["bindings"]) == {key, "other"}
    contract = schema.model_json_schema()["$defs"]["_RequiredBindings"]
    assert set(contract["required"]) == {key, "other"}
    assert contract["additionalProperties"] is False


def _objects(schema):
    if isinstance(schema, dict):
        if schema.get("type") == "object":
            yield schema
        for value in schema.values():
            yield from _objects(value)
    elif isinstance(schema, list):
        for value in schema:
            yield from _objects(value)


def test_identity_response_declares_every_property_it_needs():
    # Gemini through the Vercel AI Gateway returned a free-keyed identities map empty, so every answer whose
    # bindings pointed into it was rejected on that route alone.
    schema = _identity_schema(("output_0", "output_1"))
    wire = strict_schema(schema.model_json_schema())
    assert all(node.get("additionalProperties") is False and node["properties"] for node in _objects(wire))
    response = schema.model_validate(
        {
            "identities": [{"id": "i0", "source_ref": "q0", "quote": "Clerkenwell Coffee"}],
            "bindings": {key: {"scope": "entities", "identity_ids": ["i0"]} for key in ("output_0", "output_1")},
        }
    )
    assert response.model_dump()["identities"] == ({"id": "i0", "source_ref": "q0", "quote": "Clerkenwell Coffee"},)


@pytest.mark.parametrize("judgment", ["yes", "no", "uncertain"])
def test_assessment_response_requires_the_requested_judgment(judgment):
    schema = _assessment_schema("arbitrary-field")
    with pytest.raises(ValidationError):
        schema.model_validate({"judgments": {}, "reason": "No entry supplied."})
    with pytest.raises(ValidationError):
        schema.model_validate({"judgments": {"different": judgment}, "reason": "Wrong entry supplied."})
    response = schema.model_validate({"judgments": {"arbitrary-field": judgment}, "reason": "Evidence verdict."})
    assert response.model_dump()["judgments"] == {"arbitrary-field": judgment}
    contract = schema.model_json_schema()["$defs"]["_RequiredJudgments"]
    assert contract["required"] == ["arbitrary-field"]
    assert contract["additionalProperties"] is False
