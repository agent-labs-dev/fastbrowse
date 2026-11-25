import json

import httpx
import pytest
from fastbrowse.evals.live_tasks import Release
from parallax.models import Capability
from parallax.task import task_from_record

from fastbrowse_evals import hosted_api
from fastbrowse_evals.task_specs import record, secrets_origin, spec_for, start_url


def test_a_login_task_names_its_secret_variables_and_needs_protected_credentials() -> None:
    login = record(spec_for("saucedemo-checkout"))
    assert login.secrets == {
        "username": "FASTBROWSE_EVAL_SAUCEDEMO_USERNAME",
        "password": "FASTBROWSE_EVAL_SAUCEDEMO_PASSWORD",
    }
    assert Capability.CREDENTIALS in login.requires
    assert login.authorizes and Capability.AUTHORIZATION not in login.requires
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


def test_an_official_suite_task_starts_at_its_website() -> None:
    task = task_from_record("fastbrowse", record(spec_for("saucedemo-checkout")))
    official = task.model_copy(update={"data": {"website": "https://www.example.org/"}})
    assert start_url(official) == "https://www.example.org/"
    assert start_url(task) == task.data["start_url"]


def test_a_task_naming_no_page_starts_blank() -> None:
    # BU Bench tasks name no website: the agent begins in a blank browser rather than the trial failing to start.
    task = task_from_record("fastbrowse", record(spec_for("saucedemo-checkout")))
    assert start_url(task.model_copy(update={"data": {}})) is None


def test_a_login_is_typed_only_on_the_site_it_declares() -> None:
    task = task_from_record("fastbrowse", record(spec_for("expandtesting-login")))
    assert secrets_origin(task) == "https://practice.expandtesting.com"
    with pytest.raises(ValueError, match="declares no login_origin"):
        secrets_origin(task.model_copy(update={"data": {}}))


def test_a_task_that_passes_by_pausing_requires_authorization() -> None:
    pause = record(spec_for("saucedemo-pause"))
    assert not pause.authorizes and Capability.AUTHORIZATION in pause.requires
    assert Capability.AUTHORIZATION not in record(spec_for("saucedemo-checkout")).requires


@pytest.mark.parametrize(
    "task_id",
    [
        "stretch-date-range-monday",
        "stretch-datepicker-last-saturday",
        "stretch-datepicker-last-sunday",
        "stretch-calendar-first-friday",
        "google-flights",
        "flights-search",
    ],
)
def test_a_relative_date_task_gives_every_arm_the_resolved_date(task_id: str) -> None:
    assert "{today" in record(spec_for(task_id)).prompt


@pytest.mark.parametrize("task_id", ["pypi-version", "pypi-structured", "pypi-newer", "httpx-requires-python"])
def test_a_pypi_task_asks_for_the_stable_release(task_id: str) -> None:
    assert "stable (non pre-release)" in record(spec_for(task_id)).prompt
    assert "stable release" in str(record(spec_for(task_id)).data["intended_outcome"])


STATIC = [
    "stretch-books-sequential-art-one-star",
    "stretch-books-young-adult-one-star",
    "stretch-books-nonfiction-five-star",
    "stretch-quotes-top-authors",
    "quotes-einstein-count",
    "quotes-rowling-count",
    "books-mystery-cheapest",
    "books-travel-priciest",
    "stretch-bstack-apple-samsung",
    "stretch-bstack-apple-google",
    "table-largest-due",
    "table-total-due",
]


@pytest.mark.parametrize("task_id", STATIC)
def test_a_static_site_task_records_its_intended_outcome_for_the_judge(task_id: str) -> None:
    outcome = record(spec_for(task_id)).data["intended_outcome"]
    assert isinstance(outcome, str) and outcome
    assert task_from_record("fastbrowse", record(spec_for(task_id))).data["intended_outcome"] == outcome


def test_the_heldout_suite_holds_tasks_no_other_suite_tunes_on() -> None:
    from fastbrowse_evals.catalog import SUITES

    heldout = SUITES["heldout"]
    assert len(heldout) >= 6
    assert {spec.id for spec in heldout}.isdisjoint(
        spec.id for name, tasks in SUITES.items() if name != "heldout" for spec in tasks
    )
    for spec in heldout:
        assert record(spec).data["intended_outcome"]
        assert spec.arms == ("fastbrowse", "browser-use")
