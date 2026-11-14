import json

import httpx
import pytest
from fastbrowse.evals.live_tasks import Release
from parallax.models import Capability
from parallax.task import task_from_record

from fastbrowse_evals import hosted_api
from fastbrowse_evals.task_specs import record, spec_for


def test_a_login_task_names_its_secret_variables_and_needs_protected_credentials() -> None:
    login = record(spec_for("saucedemo-checkout"))
    assert login.secrets == {
        "username": "FASTBROWSE_EVAL_SAUCEDEMO_USERNAME",
        "password": "FASTBROWSE_EVAL_SAUCEDEMO_PASSWORD",
    }
    assert {Capability.CREDENTIALS, Capability.AUTHORIZATION} <= login.requires
    assert task_from_record("fastbrowse", login).secrets == login.secrets


def test_a_structured_task_declares_the_schema_of_the_model_the_agent_fills() -> None:
    assert record(spec_for("pypi-structured")).answer_schema == Release.model_json_schema()


async def test_the_hosted_run_types_each_login_only_on_the_start_origin(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FASTBROWSE_HOSTED_API_KEY", "fbh_test_key")
    monkeypatch.setenv("FASTBROWSE_EVAL_SAUCEDEMO_USERNAME", "standard_user")
    monkeypatch.setenv("FASTBROWSE_EVAL_SAUCEDEMO_PASSWORD", "secret_sauce")
    sent: list[dict[str, object]] = []

    def reply(request: httpx.Request) -> httpx.Response:
        sent.append(json.loads(request.content))
        return httpx.Response(200, json={"id": "run_1", "state": "complete"})

    task = task_from_record("fastbrowse", record(spec_for("saucedemo-checkout")))
    async with httpx.AsyncClient(transport=httpx.MockTransport(reply)) as http:
        await hosted_api.submit_and_wait(http, task, task.turns[0].text)
    [body] = sent
    assert body["secrets"] == [
        {"name": "username", "value": "standard_user", "origins": ["https://www.saucedemo.com"]},
        {"name": "password", "value": "secret_sauce", "origins": ["https://www.saucedemo.com"]},
    ]
    assert body["authorize"] is True
    assert body["output_schema"] is None
