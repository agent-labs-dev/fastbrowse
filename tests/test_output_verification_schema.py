import pytest
from pydantic import ValidationError

from fastbrowse.verification import _assessment_schema, _identity_schema


@pytest.mark.parametrize("key", ["criterion_17", "arbitrary-field"])
def test_identity_response_requires_each_requested_key(key):
    schema = _identity_schema((key, "other"))
    with pytest.raises(ValidationError):
        schema.model_validate({"identities": {}, "bindings": {}})
    with pytest.raises(ValidationError):
        schema.model_validate({"identities": {}, "bindings": {key: {"scope": "unresolved"}}})
    response = schema.model_validate(
        {"identities": {}, "bindings": {key: {"scope": "unresolved"}, "other": {"scope": "subjectless"}}}
    )
    assert set(response.model_dump()["bindings"]) == {key, "other"}
    contract = schema.model_json_schema()["$defs"]["_RequiredBindings"]
    assert set(contract["required"]) == {key, "other"}
    assert contract["additionalProperties"] is False


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
