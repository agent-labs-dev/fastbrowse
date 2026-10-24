"""Write what pydantic answers to model responses, valid and invalid, for Rust's response validation to match.

    uv run python crates/fastbrowse-agent/tests/golden/generate_validation.py

A client that cannot validate a structured response asks again with pydantic's error text,
`ValidationError.json(include_input=False, include_url=False)`, so a Rust re-ask body only replays when that text
matches byte for byte. Each case records the model, its schema as `json.dumps` writes it, the response text, and
either the valid `model_dump(mode="json")` or the error text.
"""

import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, Field, ValidationError, create_model

from fastbrowse.models import Frozen
from fastbrowse.planner import Plan, _AnswerChecks, _FinishOutputs
from fastbrowse.retrieval import (
    Claim,
    _AnswerDraft,
    _CollectionResponse,
    _ListOrder,
    _ReadResponse,
    _RecordsResponse,
    _TextProposals,
)
from fastbrowse.shortcut import Shortcut, StartPage
from fastbrowse.verification import LLMVerdict, _assessment_schema, _identity_schema

HERE = Path(__file__).parent

FIELD_ANSWER_DRAFT = create_model(
    "_FieldAnswerDraft",
    __base__=Frozen,
    output_0=(tuple[Claim, ...], Field(description="The price of the first item")),
    output_2=(tuple[Claim, ...], Field(description="The name of the seller")),
)
IDENTITIES = _identity_schema(["output_0"])
ASSESSMENT = _assessment_schema("output_0")

CITE = {"first": "main/:1", "last": "main/:2"}
CLAIM = {"cite": CITE, "text": "The price is $5", "requirement_id": "r1"}

CASES: dict[type[BaseModel], list[Any]] = {
    Plan: [
        {"requirements": [{"id": "r1", "text": "Find it", "kind": "information"}], "answer_expected": True},
        {"requirements": [{"id": "r1", "text": "Find it", "kind": "information"}], "answer_expected": "yes"},
        {"requirements": [{"id": "r1", "text": "Find it", "kind": "information"}], "answer_expected": 1},
        {"requirements": [], "answer_expected": "maybe"},
        {"requirements": [{"id": "", "text": "Find it", "kind": "lookup"}], "answer_expected": True},
        {"requirements": [{"id": "r1", "text": "x", "kind": "action", "extra": 1}], "answer_expected": True},
        {"requirements": "none", "answer_expected": True},
        {"answer_expected": True, "note": "x"},
        [],
        "plan",
        None,
        {
            "requirements": [{"id": "r1", "text": "a", "kind": "action"}, {"id": "r1", "text": "b", "kind": "action"}],
            "answer_expected": False,
        },
        {"requirements": [], "answer_expected": False, "run_reports": ["final_url", "map"]},
        {"requirements": [], "answer_expected": False, "answer_checks": [str(i) for i in range(130)]},
        {"requirements": [{"id": 1, "text": None, "kind": "action", "count_records": "no"}], "answer_expected": 0},
        {"requirements": [{"id": "r", "text": "t", "kind": "action", "count_records": 2}], "answer_expected": 1.0},
        {"requirements": [{"id": "r", "text": "t", "kind": "action", "count_records": 1.5}], "answer_expected": True},
    ],
    _AnswerChecks: [
        {"checks": ["The price"]},
        {"checks": []},
        {"checks": ["  "]},
        {"checks": [1, None]},
        {"checks": "price"},
    ],
    _FinishOutputs: [
        {
            "groups": [
                {"subjects": [{"name": "A", "requirement_ids": ["r1"]}], "fields": ["price"], "check_indices": [0]}
            ]
        },
        {"groups": [{"subjects": [], "fields": [], "check_indices": ["0", "x", 1.0, 1.5, True]}]},
        {"groups": [{"subjects": [{"name": "", "requirement_ids": []}], "fields": ["p"]}]},
        {"groups": []},
    ],
    Shortcut: [
        {"url": "https://a.test/"},
        {"url": None},
        {},
        {"url": 5},
        {"url": "x", "why": "y"},
        {"url": ["a"]},
        {"why": "y", "url": 5},
    ],
    StartPage: [{"url": "https://a.test/"}, {"url": True}],
    _ReadResponse: [
        {"claims": [CLAIM], "answered": True},
        {"claims": [{**CLAIM, "excerpt": ""}], "answered": True},
        {"claims": [{**CLAIM, "excerpt": "x" * 2001}], "answered": True},
        {"claims": [{"text": "t"}], "answered": "no"},
        {"claims": [{"cite": {"first": "a"}, "text": 5, "draws_on": "e1"}], "answered": True},
        {"claims": [{"cite": None, "text": "t", "records": [{"first": "a", "last": "b", "x": 1}]}], "answered": True},
        {"claims": [], "answered": True, "continues": [{"requirement_id": "r1", "expands": None}]},
        {
            "claims": [],
            "answered": False,
            "continues": [
                {
                    "requirement_id": "r1",
                    "comparison": {
                        "order": "cheapest",
                        "limit": 0,
                        "label": {"prefix": "", "suffix": ""},
                        "value": {"prefix": ""},
                    },
                    "tallies": [{"key": None, "field": {"span": CITE, "prefix": "by ", "suffix": "\n"}}],
                    "through_end": "true",
                    "reuse_field": "maybe",
                }
            ],
        },
        {
            "claims": [],
            "answered": False,
            "continues": [
                {
                    "requirement_id": "r1",
                    "comparison": {
                        "order": "lowest",
                        "limit": 61,
                        "label": {"prefix": "", "suffix": ""},
                        "value": {"prefix": "", "suffix": ""},
                    },
                }
            ],
        },
        {"claims": [], "answered": True, "tallies": [{"requirement_id": "r1", "groups": [{"key": 3}]}]},
        {"answered": True, "summary": "x"},
        {"claims": {"0": CLAIM}, "answered": True},
    ],
    _CollectionResponse: [
        {"claims": [], "answered": True, "collections": [{"requirement_id": "r1", "scope": "s", "records": [CITE]}]},
        {"claims": [], "answered": True, "collections": [{"requirement_id": "r1", "records": [{"last": "x"}]}]},
    ],
    _RecordsResponse: [
        {"continues": [{"requirement_id": "r1", "records": [CITE]}], "ended": ["r1"]},
        {"continues": [{"records": [CITE]}], "requested_scope_complete": "r1", "context": [1]},
    ],
    _TextProposals: [
        {"fields": [{"field": "name", "value": "httpx", "source_id": "main/:1"}]},
        {"fields": [{"field": "name", "value": 0.28, "source_id": 3}]},
        {"fields": None},
    ],
    _ListOrder: [{"confirmed": True}, {"confirmed": "false"}, {"confirmed": "nah"}, {"confirmed": 0}, {}],
    _AnswerDraft: [
        {"claims": [{"text": "t", "evidence_ids": ["e1"]}]},
        {"claims": [{"text": "t", "evidence_ids": "e1", "ranks": "y"}]},
        {"claims": [{"evidence_ids": []}]},
    ],
    FIELD_ANSWER_DRAFT: [
        {"output_0": [{"text": "t", "evidence_ids": ["e1"]}], "output_2": []},
        {"output_0": [{"text": "t"}]},
        {"output_0": [], "output_1": [], "output_2": []},
    ],
    LLMVerdict: [
        {"missing": [], "complete": True},
        {"missing": [], "ungrounded": ["r1"], "complete": True},
        {"missing": "r1", "complete": "unsure"},
        {"complete": True},
    ],
    IDENTITIES: [
        {
            "identities": [{"id": "i1", "source_ref": "q0", "quote": "Acme"}],
            "bindings": {"output_0": {"scope": "entities", "identity_ids": ["i1"]}},
        },
        {"identities": [], "bindings": {}},
        {"identities": [{"id": "i1"}], "bindings": {"output_0": {"scope": "everyone"}, "output_1": {}}},
    ],
    ASSESSMENT: [
        {"judgments": {"output_0": "yes"}, "reason": "ok"},
        {"judgments": {"output_0": "Yes"}, "reason": "ok"},
        {"judgments": {}, "reason": None},
        {"judgments": {"output_0": "no", "output_1": "no"}},
    ],
}

RAW = [
    '{"claims": [',
    '{"claims": [], "answered": tru',
    "",
    "   ",
    '{"x": 1} trailing',
    "[1, 2",
    '{"a": "b\\q"}',
    "nul",
]


def outcome(model: type[BaseModel], text: str) -> dict[str, Any]:
    try:
        return {"ok": model.model_validate_json(text).model_dump(mode="json")}
    except ValidationError as error:
        return {"errors": error.json(include_input=False, include_url=False)}


def main() -> None:
    cases = []
    for model, inputs in CASES.items():
        texts = [json.dumps(value) for value in inputs]
        if model is _ReadResponse:
            texts.extend(RAW)
        for text in texts:
            cases.append(
                {
                    "model": model.__name__,
                    "schema": json.dumps(model.model_json_schema()),
                    "input": text,
                    "result": outcome(model, text),
                }
            )
    (HERE / "validation.json").write_text(json.dumps(cases, indent=1) + "\n", encoding="ascii")


if __name__ == "__main__":
    main()
