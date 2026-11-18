"""The decisions of the automatic cassette refill: which tasks missed, whether to spend, whether it worked."""

import json
from dataclasses import replace
from pathlib import Path
from typing import Any

import pytest
import yaml

from fastbrowse.evals.replay import CASSETTE_VERSION, Cassette, Exchange, request_key
from scripts import cassette_refill as refill

SHA = "a" * 40
REPO = "agent-labs-dev/fastbrowse"


def pull(**changes: Any) -> refill.PullFacts:
    return replace(
        refill.PullFacts(
            number=7,
            open=True,
            head_sha=SHA,
            head_repo=REPO,
            base_repo=REPO,
            head_message="feat(core): ask something else",
        ),
        **changes,
    )


def decide(
    facts: refill.PullFacts, tasks: list[str] | None = None, attempted: list[str] | None = None, retry: bool = False
) -> refill.Decision:
    wanted = ["mock-basket"] if tasks is None else tasks
    return refill.refill_decision(facts, run_sha=SHA, tasks=wanted, attempted=attempted or [], retry=retry)


# Which tasks missed


def test_missed_tasks_come_from_the_files_the_replay_check_wrote(tmp_path: Path) -> None:
    refill.record_misses(tmp_path, "mock-basket", [{"purpose": "jev:a", "model": "m", "diff": "x"}])
    refill.record_misses(tmp_path, "contact-form", [{"purpose": "llm:b", "model": None, "diff": "y"}])
    refill.record_misses(tmp_path, "login-wall", [])
    assert refill.missed_tasks(tmp_path) == ["contact-form", "mock-basket"]
    assert json.loads((tmp_path / "mock-basket.json").read_text())["misses"] == [{"purpose": "jev:a", "model": "m"}]


def test_a_replay_with_no_misses_and_a_missing_directory_name_no_task(tmp_path: Path) -> None:
    assert refill.missed_tasks(tmp_path / "absent") == []
    assert refill.missed_tasks(tmp_path) == []


@pytest.mark.parametrize("name", ["../x", "Bad_Id", "-lead", "has space", "a;b"])
def test_a_missed_task_name_that_is_not_a_task_id_is_refused(tmp_path: Path, name: str) -> None:
    (tmp_path / "x.json").write_text(json.dumps({"task": name, "misses": [{"purpose": "p"}]}))
    with pytest.raises(ValueError, match="not a record of a missed task"):
        refill.missed_tasks(tmp_path)


def test_a_file_named_for_another_task_than_it_records_is_refused(tmp_path: Path) -> None:
    (tmp_path / "mock-basket.json").write_text(json.dumps({"task": "contact-form", "misses": [{"purpose": "p"}]}))
    with pytest.raises(ValueError):
        refill.missed_tasks(tmp_path)


# Whether to spend


def test_a_same_repo_pull_request_with_misses_is_refilled() -> None:
    assert decide(pull()) == refill.Decision(True, "refill mock-basket")


def test_a_fork_is_never_run_with_the_model_key() -> None:
    decision = decide(pull(head_repo="someone/fastbrowse"))
    assert not decision.run and "fork" in decision.reason


def test_a_closed_pull_request_is_not_refilled() -> None:
    assert not decide(pull(open=False)).run


def test_a_check_run_on_an_older_head_is_not_refilled() -> None:
    decision = decide(pull(head_sha="b" * 40))
    assert not decision.run and "now at" in decision.reason


def test_the_refill_does_not_refill_its_own_commit() -> None:
    verdict = refill.fill_verdict(None, ["mock-basket"], 1.0, 0)
    message = refill.commit_message(["mock-basket"], "c" * 40, verdict)
    assert refill.is_refill_commit(message)
    assert not decide(pull(head_message=message)).run


def test_a_head_commit_is_tried_once() -> None:
    assert not decide(pull(), attempted=[SHA]).run
    assert decide(pull(), attempted=["c" * 40]).run


def test_a_maintainer_retry_lifts_only_the_attempted_guard() -> None:
    assert decide(pull(), attempted=[SHA], retry=True).run
    assert not decide(pull(head_repo="someone/fastbrowse"), attempted=[SHA], retry=True).run
    assert not decide(pull(head_message=f"x\n\n{refill.TRAILER}: {SHA}\n"), retry=True).run


def test_no_missed_task_means_no_spend() -> None:
    assert not decide(pull(), tasks=[]).run


def test_the_attempted_record_survives_a_round_trip_through_the_comment() -> None:
    body = refill.render_comment(
        head_sha=SHA, tasks=["mock-basket"], verdict=None, outcome="Filling.", attempted=["b" * 40, SHA], run_url="u"
    )
    assert refill.MARKER in body
    assert refill.attempted_shas(body) == ["b" * 40, SHA]
    assert refill.attempted_shas("a human wrote this") == []


# Whether the fill worked


def entry(task: str = "mock-basket", **changes: Any) -> dict[str, Any]:
    return {
        "task": task,
        "recorded_cost_usd": 0.25,
        "unpriced_exchanges": 0,
        "misses": [],
        "upstream_errors": [],
        "result": {"grade": {"passed": True, "failure": None}},
        **changes,
    }


def verdict(*entries: dict[str, Any], tasks: tuple[str, ...] = ("mock-basket",), budget: float = 1.0, code: int = 0):
    return refill.fill_verdict({"tasks": list(entries)}, list(tasks), budget, code)


def test_a_clean_fill_within_budget_is_accepted() -> None:
    result = verdict(entry())
    assert result.accepted and result.spent == 0.25 and result.complete and result.converged


@pytest.mark.parametrize(
    ("changes", "reason"),
    [
        ({"result": {"grade": {"passed": False, "failure": "wrong total"}}}, "grade failed: wrong total"),
        ({"misses": [{"purpose": "p"}]}, "1 replay misses remained"),
        ({"upstream_errors": [429]}, "upstream refused 1 requests [429]"),
        ({"budget_refusals": 3}, "the budget stopped 3 requests before they were sent"),
        ({"unpriced_exchanges": 2}, "2 exchanges have no price"),
    ],
)
def test_a_task_that_fails_for_any_reason_rejects_the_fill_and_names_it(changes: dict[str, Any], reason: str) -> None:
    result = verdict(entry(**changes))
    assert not result.accepted
    assert result.tasks[0].reasons == (reason,)


def test_a_fill_over_budget_is_rejected_even_when_every_task_passed() -> None:
    result = verdict(entry(recorded_cost_usd=1.5))
    assert not result.accepted and result.tasks[0].passed and "over budget" in result.reasons[0]


def test_a_fill_pass_that_was_paid_for_but_not_kept_counts_against_the_budget() -> None:
    result = verdict(entry(recorded_cost_usd=0.25, spent_usd=1.5))
    assert not result.accepted and result.spent == 1.5 and "over budget" in result.reasons[0]


def test_a_task_the_fill_never_reached_is_rejected() -> None:
    result = verdict(entry(), tasks=("mock-basket", "contact-form"))
    assert not result.accepted and not result.complete
    assert [task.passed for task in result.tasks] == [True, False]


def test_a_nonzero_exit_or_a_missing_report_is_rejected() -> None:
    assert not verdict(entry(), code=1).accepted
    assert not refill.fill_verdict(None, ["mock-basket"], 1.0, 0).accepted


def test_a_fill_that_ran_an_unrequested_task_is_rejected() -> None:
    assert not verdict(entry(), entry("login-wall")).accepted


@pytest.mark.parametrize("value", ["", "0", "-1", "nan", "inf"])
def test_a_budget_is_finite_and_positive(value: str) -> None:
    with pytest.raises(ValueError):
        refill.positive(value)


# What is committed


def write_cassette(directory: Path, task: str, *statuses: int) -> None:
    """A cassette as `bench` saves it, so the check is held to what replay itself writes and reads."""
    request = {"model": "m", "messages": []}
    exchanges = [
        Exchange(
            key=request_key("/chat/completions", request),
            path="/chat/completions",
            purpose="plan",
            model="m",
            request=request,
            status=status,
            content_type="application/json",
            body="{}",
            latency=0.1,
        )
        for status in statuses
    ]
    Cassette(task=task, exchanges=exchanges).save(directory / f"{task}.jsonl")


def test_the_committing_job_reads_the_cassette_version_replay_reads() -> None:
    assert refill.CASSETTE_VERSION == CASSETTE_VERSION


def test_a_cassette_of_another_version_is_never_committed(tmp_path: Path) -> None:
    write_cassette(tmp_path, "mock-basket", 200)
    path = tmp_path / "mock-basket.jsonl"
    head, _, rest = path.read_text().partition("\n")
    path.write_text(json.dumps({**json.loads(head), "cassette": CASSETTE_VERSION + 1}) + "\n" + rest)
    with pytest.raises(ValueError, match="head line"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


@pytest.mark.parametrize("field", ["path", "purpose", "status", "content_type", "body", "latency", "request"])
def test_a_cassette_with_an_exchange_replay_cannot_read_is_never_committed(tmp_path: Path, field: str) -> None:
    write_cassette(tmp_path, "mock-basket", 200)
    path = tmp_path / "mock-basket.jsonl"
    head, exchange = path.read_text().splitlines()
    path.write_text(head + "\n" + json.dumps({k: v for k, v in json.loads(exchange).items() if k != field}) + "\n")
    with pytest.raises(ValueError, match="exchange"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


def test_only_the_requested_cassettes_are_committed(tmp_path: Path) -> None:
    write_cassette(tmp_path, "mock-basket", 200, 200)
    (tmp_path / "result.json").write_text("{}")
    assert [path.name for path in refill.validate_cassettes(tmp_path, ["mock-basket"])] == ["mock-basket.jsonl"]


def test_a_cassette_holding_an_upstream_error_is_never_committed(tmp_path: Path) -> None:
    write_cassette(tmp_path, "mock-basket", 200, 429)
    with pytest.raises(ValueError, match="429"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


@pytest.mark.parametrize("extra", ["workflow.yml", "other.jsonl"])
def test_a_file_that_was_not_asked_for_is_never_committed(tmp_path: Path, extra: str) -> None:
    write_cassette(tmp_path, "mock-basket", 200)
    (tmp_path / extra).write_text("x")
    with pytest.raises(ValueError, match="expected exactly"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


def test_a_cassette_for_another_task_than_its_name_is_refused(tmp_path: Path) -> None:
    write_cassette(tmp_path, "mock-basket", 200)
    (tmp_path / "mock-basket.jsonl").write_text(json.dumps({"cassette": 1, "task": "contact-form"}) + "\n")
    with pytest.raises(ValueError, match="head line"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


def test_the_comment_names_each_task_its_cost_and_why_it_failed() -> None:
    result = verdict(
        entry(),
        entry("login-wall", misses=[{"purpose": "p"}], recorded_cost_usd=None),
        tasks=("mock-basket", "login-wall"),
    )
    body = refill.render_comment(
        head_sha=SHA,
        tasks=["mock-basket", "login-wall"],
        verdict=result,
        outcome="Nothing was committed.",
        attempted=[SHA],
        run_url="u",
    )
    assert "| `mock-basket` | passed | $0.2500 | - |" in body
    assert "| `login-wall` | failed | unknown | 1 replay misses remained |" in body


def test_a_cassette_that_is_a_link_is_never_committed(tmp_path: Path) -> None:
    (tmp_path / "secret").write_text("x")
    (tmp_path / "mock-basket.jsonl").symlink_to(tmp_path / "secret")
    with pytest.raises(ValueError, match="regular file"):
        refill.validate_cassettes(tmp_path, ["mock-basket"])


# The workflow keeps its trust model


def workflow() -> dict[str, Any]:
    return yaml.safe_load((Path(__file__).parents[1] / ".github/workflows/cassette-refill.yml").read_text())


def test_only_the_job_that_reads_the_fill_as_data_holds_the_commit_token() -> None:
    jobs = workflow()["jobs"]
    holders = {name for name, job in jobs.items() if "AUR_SYNC_TOKEN" in json.dumps(job)}
    assert holders == {"publish"}
    assert {name for name, job in jobs.items() if "OPENROUTER_API_KEY" in json.dumps(job)} == {"fill"}


def test_the_job_that_runs_the_pull_requests_code_can_only_read_and_keeps_no_credential() -> None:
    fill = workflow()["jobs"]["fill"]
    assert fill["permissions"] == {"contents": "read"}
    checkout = next(step for step in fill["steps"] if str(step.get("uses", "")).startswith("actions/checkout"))
    assert checkout["with"]["persist-credentials"] is False


@pytest.mark.parametrize("job", ["plan", "publish"])
def test_the_jobs_that_hold_a_write_token_run_the_default_branchs_code(job: str) -> None:
    """A dispatch from a branch checked out that branch, so its script ran with the commit token."""
    steps = workflow()["jobs"][job]["steps"]
    checkout = next(step for step in steps if str(step.get("uses", "")).startswith("actions/checkout"))
    assert checkout["with"]["ref"] == "${{ github.event.repository.default_branch }}"
    assert checkout["with"]["persist-credentials"] is False


def test_a_pull_request_from_a_fork_never_starts_the_refill() -> None:
    condition = workflow()["jobs"]["plan"]["if"]
    assert "workflow_run.head_repository.full_name == github.repository" in condition
    assert "workflow_run.event == 'pull_request'" in condition and "conclusion == 'failure'" in condition


def test_the_replay_job_hands_the_missed_tasks_to_the_refill_as_files() -> None:
    steps = yaml.safe_load((Path(__file__).parents[1] / ".github/workflows/ci.yml").read_text())["jobs"]["replay"][
        "steps"
    ]
    test = next(step for step in steps if "test_native_replay" in str(step.get("run", "")))
    assert "FASTBROWSE_REPLAY_MISSES" in test["env"]
    upload = next(step for step in steps if str(step.get("uses", "")).startswith("actions/upload-artifact"))
    assert upload["with"]["name"] == "replay-misses" and upload["if"] == "always()"


# Review findings


def test_the_refill_comment_is_found_past_the_first_page_of_comments(monkeypatch: pytest.MonkeyPatch) -> None:
    ours = {"id": 9, "body": f"{refill.MARKER}\n", "user": {"login": refill.BOT}}
    forged = {"id": 8, "body": refill.MARKER, "user": {"login": "someone"}}
    pages = [[{"id": n, "body": "talk", "user": {"login": "u"}} for n in range(100)], [forged, ours]]
    seen: list[tuple[str, ...]] = []

    def gh(*args: str, token: str, body: Any = None) -> Any:
        seen.append(args)
        return pages

    monkeypatch.setattr(refill, "gh", gh)
    assert refill.find_comment(REPO, "7", "token") == ours
    assert "--paginate" in seen[0] and "--slurp" in seen[0]


def test_what_bench_writes_beside_its_report_is_not_in_the_artifact_that_is_committed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("REFILL_TASKS", '["mock-basket"]')
    monkeypatch.setenv("REFILL_BUDGET", "1")
    cassettes = tmp_path / "bench/cassettes"
    cassettes.mkdir(parents=True)
    write_cassette(cassettes, "mock-basket", 200)

    def bench(command: list[str], check: bool) -> Any:
        report = Path(command[command.index("--out") + 1])
        report.write_text(json.dumps({"tasks": [entry()]}))
        (report.with_suffix("") / "logs").mkdir(parents=True)  # as `bench` does beside any --out
        return type("Done", (), {"returncode": 0})()

    monkeypatch.setattr(refill.subprocess, "run", bench)
    assert refill.fill() == 0
    assert [path.name for path in refill.validate_cassettes(refill.OUT, ["mock-basket"])] == ["mock-basket.jsonl"]
    assert json.loads((refill.OUT / "result.json").read_text())["report"]["tasks"][0]["task"] == "mock-basket"
