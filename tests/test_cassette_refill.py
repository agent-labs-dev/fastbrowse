"""The decisions of the automatic cassette refill: which tasks missed, whether to spend, whether it worked."""

import base64
import hashlib
import json
import math
import subprocess
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


def test_a_head_commit_is_tried_once() -> None:
    assert not decide(pull(), attempted=[SHA]).run
    assert decide(pull(), attempted=["c" * 40]).run


def test_a_maintainer_retry_lifts_only_the_attempted_guard() -> None:
    assert decide(pull(), attempted=[SHA], retry=True).run
    assert not decide(pull(head_repo="someone/fastbrowse"), attempted=[SHA], retry=True).run


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


@pytest.mark.parametrize("spent", [float("nan"), float("inf"), -1.0, "0", True])
def test_a_spend_that_is_not_a_dollar_amount_rejects_the_fill(spent: Any) -> None:
    result = verdict(entry(spent_usd=spent))
    assert not result.accepted and not result.tasks[0].passed and result.tasks[0].cost is None
    assert any("not a dollar amount" in reason for reason in result.reasons)


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


def cassette_tree(route: str, filename: str, blob: str) -> dict[str, Any]:
    entries = {
        "root": {"path": "bench", "type": "tree", "sha": "bench"},
        "bench": {"path": "cassettes", "type": "tree", "sha": "cassettes"},
        "cassettes": {"path": filename, "type": "blob", "mode": "100644", "sha": blob},
    }
    return {"truncated": False, "tree": [entries[route.rsplit("/", 1)[1]]]}


@pytest.mark.parametrize("empty_directory", [False, True])
def test_partial_retry_requires_the_replay_miss_records(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, empty_directory: bool
) -> None:
    misses = tmp_path / "replay-misses"
    if empty_directory:
        misses.mkdir()
    env = {"REPO": REPO, "PR": "7", "SHA": SHA, "GH_TOKEN": "t", "MISSES_DIR": str(misses)}
    for name, value in env.items():
        monkeypatch.setenv(name, value)
    monkeypatch.setenv("COVERED_TASKS", '["mock-basket"]')
    with pytest.raises(ValueError, match="does not have a replay miss for every task"):
        refill.plan()


@pytest.mark.parametrize(
    ("rebased", "split", "comment_records_head", "retry", "run_head_matches", "runs"),
    [
        (False, False, False, False, True, False),
        (False, True, False, False, True, False),
        (True, False, False, False, True, True),
        (True, True, False, False, True, True),
        (True, False, True, True, True, False),
        (True, True, False, True, False, False),
    ],
)
def test_plan_uses_the_exact_refill_commit_and_ci_head(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    rebased: bool,
    split: bool,
    comment_records_head: bool,
    retry: bool,
    run_head_matches: bool,
    runs: bool,
) -> None:
    source, head, middle, new_base = ("a" * 40, "b" * 40, "c" * 40, "d" * 40)
    base = new_base if rebased else source
    terminal = f"{refill.REFILL_SUBJECT}mock-basket\n\n{refill.TRAILER}: {source}"
    history = {
        head: (middle if split else base, "mock-basket", terminal),
        middle: (base, "mock-archive", f"{refill.REFILL_SUBJECT}mock-archive"),
        new_base: ("e" * 40, "", "fix: update the base"),
    }
    comment_head = head if comment_records_head else "f" * 40
    comment = (
        f"{refill.MARKER}\n<!-- cassette-refill-attempted: {source} -->\n"
        f"Committed the refilled cassettes as [{comment_head[:12]}]"
        f"(https://github.com/{REPO}/commit/{comment_head}); CI runs again on it."
    )
    env = {
        "REPO": REPO,
        "PR": "7",
        "SHA": head if run_head_matches else new_base,
        "GH_TOKEN": "t",
        "RETRY": str(retry).lower(),
        "MISSES_DIR": str(tmp_path),
        "COVERED_TASKS": "[]",
        "RUN_URL": "test-run",
    }
    for name, value in env.items():
        monkeypatch.setenv(name, value)
    refill.record_misses(tmp_path, "mock-basket", [{"purpose": "done", "model": "jev"}])
    facts = pull(head_sha=head, head_message=terminal)
    monkeypatch.setattr(refill, "pull_facts", lambda *args: (facts, "branch"))
    monkeypatch.setattr(refill, "find_comment", lambda *args: {"body": comment})
    monkeypatch.setattr(refill, "summary", lambda *args: None)
    outputs: dict[str, str] = {}
    claims: list[str] = []
    monkeypatch.setattr(refill, "output", lambda **values: outputs.update(values))
    monkeypatch.setattr(refill, "upsert_comment", lambda repo, number, token, body: claims.append(body))

    def read_commit(repo: str, sha: str, token: str) -> dict[str, Any]:
        parent, task, message = history[sha]
        return {
            "commit": {"message": message, "verification": {"verified": True}},
            "committer": {"login": "web-flow"},
            "parents": [{"sha": parent}],
            "files": [{"filename": f"bench/cassettes/{task}.jsonl", "status": "modified", "sha": "e" * 40}],
        }

    monkeypatch.setattr(refill, "read_commit", read_commit)
    assert refill.plan() == 0
    assert outputs["run"] == str(runs).lower()
    assert bool(claims) == runs
    if runs:
        assert refill.attempted_shas(claims[0]) == [source, head]


@pytest.mark.parametrize(
    ("status", "stderr", "expected"),
    [
        ("HTTP/2.0 504 Gateway Timeout", "gh: HTTP 504", refill.GitHubGatewayTimeout),
        ("HTTP/2.0 504 Gateway Timeout", "gh: Gateway Timeout (HTTP 504)", refill.GitHubGatewayTimeout),
        ("HTTP/2.0 422 Unprocessable Entity", "gh: HTTP 422", RuntimeError),
        ("HTTP/2.0 200 OK", "gh: An error message mentioned HTTP 504", RuntimeError),
        ("", "gh: could not connect", RuntimeError),
    ],
)
def test_gh_distinguishes_a_gateway_timeout_from_an_http_rejection(
    monkeypatch: pytest.MonkeyPatch, status: str, stderr: str, expected: type[RuntimeError]
) -> None:
    stdout = f'{status}\nContent-Type: application/json\n\n{{"message":"failure"}}' if status else ""

    def run(command: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        assert command == ["gh", "api", "graphql", "--include"]
        return subprocess.CompletedProcess(command, 1, stdout=stdout, stderr=stderr + "\n")

    monkeypatch.setattr(refill.subprocess, "run", run)
    with pytest.raises(expected) as raised:
        refill.gh("graphql", token="test")
    assert type(raised.value) is expected


def test_gh_reports_rate_limit_headers_and_the_wait_they_name(monkeypatch: pytest.MonkeyPatch) -> None:
    stdout = (
        "HTTP/2.0 403 Forbidden\nX-Ratelimit-Resource: graphql\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 1060\n\n{}"
    )
    result = subprocess.CompletedProcess(["gh"], 1, stdout=stdout, stderr="gh: API rate limit already exceeded\n")
    monkeypatch.setattr(refill.subprocess, "run", lambda *args, **kwargs: result)
    monkeypatch.setattr(refill.time, "time", lambda: 1000.0)
    with pytest.raises(
        refill.GitHubRateLimited, match=r"x-ratelimit-resource: graphql, x-ratelimit-remaining: 0"
    ) as raised:
        refill.gh("graphql", token="test")
    assert raised.value.wait == 60


def test_gh_names_the_headers_of_a_403_that_is_not_a_rate_limit(monkeypatch: pytest.MonkeyPatch) -> None:
    stdout = "HTTP/2.0 403 Forbidden\nX-Ratelimit-Remaining: 3909\n\n{}"
    result = subprocess.CompletedProcess(["gh"], 1, stdout=stdout, stderr="gh: refused\n")
    monkeypatch.setattr(refill.subprocess, "run", lambda *args, **kwargs: result)
    with pytest.raises(RuntimeError, match=r"x-ratelimit-remaining: 3909") as raised:
        refill.gh("graphql", token="test")
    assert not isinstance(raised.value, refill.GitHubRateLimited)


def test_commit_batch_waits_out_a_rate_limit_and_sends_the_same_commit_again(monkeypatch: pytest.MonkeyPatch) -> None:
    sent: list[Any] = []
    sleeps: list[float] = []

    def gh(*args: str, token: str, body: Any = None) -> Any:
        sent.append(body)
        if len(sent) == 1:
            raise refill.GitHubRateLimited("gh api graphql failed: rate limited", 30)
        return {"data": {"createCommitOnBranch": {"commit": {"oid": "c" * 40, "url": "u"}}}}

    monkeypatch.setattr(refill, "gh", gh)
    monkeypatch.setattr("time.sleep", sleeps.append)
    path = Path(__file__)
    made = refill.commit_batch(REPO, "7", "b", "a" * 40, "feat: x\n\nbody", [path], "t", "c")
    assert made == {"oid": "c" * 40, "url": "u"}
    assert sleeps == [31] and sent[0] == sent[1]


def test_gh_decodes_the_graphql_body_after_response_headers(monkeypatch: pytest.MonkeyPatch) -> None:
    result = subprocess.CompletedProcess(
        ["gh"], 0, stdout='HTTP/2.0 200 OK\nContent-Type: application/json\n\n{"data":{"commit":1}}', stderr=""
    )
    monkeypatch.setattr(refill.subprocess, "run", lambda *args, **kwargs: result)
    assert refill.gh("graphql", token="test") == {"data": {"commit": 1}}


def publish_split(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    refuse_commit: int | None = None,
    lose_response: int | None = None,
    wrong_blob: bool = False,
    lose_body: bool = False,
    unchanged: bool = False,
    delayed_reads: int = 0,
    sleeps: list[float] | None = None,
    timeout_commit: int | None = None,
) -> tuple[int, list, str]:
    """`publish` of three cassettes, one per commit, with GitHub refusing commit number `refuse_commit` and the
    gateway timing out on request number `timeout_commit` without applying it."""
    tasks = ["mock-basket", "mock-swatch", "mock-sign-in"]
    artifact, sha = tmp_path / "refill-result", "a" * 40
    artifact.mkdir()
    for task in tasks:
        write_cassette(artifact, task, 200)
    report = {"tasks": [entry(task, recorded_cost_usd=0.1) for task in tasks]}
    (artifact / "result.json").write_text(json.dumps({"report": report, "exit_code": 0}))
    env = {"REPO": REPO, "PR": "7", "SHA": sha, "GH_TOKEN": "t", "COMMIT_TOKEN": "c", "REFILL_TASKS": json.dumps(tasks)}
    env |= {"REFILL_BUDGET": "1", "ARTIFACT_DIR": str(artifact), "RUN_URL": "u"}
    for name, value in (env | {"GITHUB_STEP_SUMMARY": str(tmp_path / "summary")}).items():
        monkeypatch.setenv(name, value)
    # Room for the largest file's base64 alone, so each commit takes one file.
    largest = max(4 * math.ceil((artifact / f"{task}.jsonl").stat().st_size / 3) for task in tasks)
    monkeypatch.setattr(refill, "COMMIT_PAYLOAD_LIMIT", largest * (2 if unchanged else 1))
    commits: list[dict[str, Any]] = []
    posted: list[str] = []
    head = sha
    omitted: dict[str, str] = {}
    if sleeps is not None:
        monkeypatch.setattr("time.sleep", sleeps.append)

    def gh(*args: str, token: str, body: Any = None) -> Any:
        nonlocal head, delayed_reads
        if args[0] == "graphql":
            commits.append(body["variables"]["input"])
            if len(commits) == refuse_commit:
                return {"errors": [{"message": "refused"}]}
            if len(commits) == timeout_commit:
                raise refill.GitHubGatewayTimeout("gh api graphql failed: gh: HTTP 504")
            head = str(len(commits)) * 40
            if len(commits) == lose_response:
                if lose_body:
                    return {"data": {"createCommitOnBranch": None}}
                raise refill.GitHubGatewayTimeout("gh api graphql failed: gh: HTTP 504")
            return {"data": {"createCommitOnBranch": {"commit": {"oid": head, "url": f"c/{head}"}}}}
        if args[0].endswith("/pulls/7"):
            repo = {"full_name": REPO}
            visible = head
            if commits and len(commits) == lose_response and delayed_reads:
                delayed_reads -= 1
                visible = commits[-1]["expectedHeadOid"]
            return {"state": "open", "head": {"sha": visible, "repo": repo, "ref": "b"}, "base": {"repo": repo}}
        if "/git/trees/" in args[0]:
            filename, oid = next(iter(omitted.items()))
            return cassette_tree(args[0], filename, "0" * 40 if wrong_blob else oid)
        if args[0].endswith(f"/commits/{head}?per_page=100"):
            sent = commits[-1]
            files = []
            for item in sent["fileChanges"]["additions"]:
                data = base64.b64decode(item["contents"])
                oid = hashlib.sha1(f"blob {len(data)}\0".encode() + data, usedforsecurity=False).hexdigest()
                name = Path(item["path"]).name
                if unchanged and name == "mock-basket.jsonl":
                    omitted[name] = oid
                    continue
                files.append(
                    {
                        "filename": item["path"],
                        "sha": "0" * 40 if wrong_blob and not unchanged else oid,
                        "status": "modified",
                    }
                )
            return [
                {
                    "commit": {
                        "message": sent["message"]["headline"] + "\n\n" + sent["message"]["body"],
                        "verification": {"verified": True},
                        "tree": {"sha": "root"},
                    },
                    "committer": {"login": "web-flow"},
                    "parents": [{"sha": sent["expectedHeadOid"]}],
                    "files": files,
                    "html_url": f"c/{head}",
                }
            ]
        if "/commits/" in args[0]:
            return {"commit": {"message": "fix: x"}}
        if body is not None:
            posted.append(body["body"])
            return {}
        return [[]]

    monkeypatch.setattr(refill, "gh", gh)
    return refill.publish(), commits, posted[-1]


def test_a_refill_over_one_request_is_committed_as_a_chain_with_the_trailer_on_its_last_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    code, commits, comment = publish_split(tmp_path, monkeypatch, refuse_commit=None)
    assert code == 0
    assert [c["expectedHeadOid"] for c in commits] == ["a" * 40, "1" * 40, "2" * 40]
    assert [[f["path"] for f in c["fileChanges"]["additions"]] for c in commits] == [
        ["bench/cassettes/mock-basket.jsonl"],
        ["bench/cassettes/mock-sign-in.jsonl"],
        ["bench/cassettes/mock-swatch.jsonl"],
    ]
    trailers = [refill.refill_source(c["message"]["body"]) is not None for c in commits]
    assert trailers == [False, False, True]
    assert f"{refill.TRAILER}: {'a' * 40}" in commits[-1]["message"]["body"]
    assert all(oid[:12] in comment for oid in ("1" * 40, "2" * 40, "3" * 40))


def test_a_split_refill_stopped_partway_says_what_it_committed_and_leaves_the_rest_to_the_next_refill(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sleeps: list[float] = []
    code, commits, comment = publish_split(tmp_path, monkeypatch, refuse_commit=2, sleeps=sleeps)
    assert sleeps == []
    assert code == 1
    assert refill.refill_source(commits[0]["message"]["body"]) is None
    assert "Committed [111111111111]" in comment and "refused" in comment
    assert "Nothing was committed" not in comment


def test_a_batch_that_times_out_unapplied_is_split_with_the_trailer_on_the_final_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sleeps: list[float] = []
    code, commits, comment = publish_split(tmp_path, monkeypatch, unchanged=True, timeout_commit=1, sleeps=sleeps)
    assert code == 0
    assert [c["expectedHeadOid"] for c in commits] == ["a" * 40, "a" * 40, "2" * 40, "3" * 40]
    assert [len(c["fileChanges"]["additions"]) for c in commits] == [2, 1, 1, 1]
    assert [refill.refill_source(c["message"]["body"]) is not None for c in commits] == [False, False, False, True]
    assert all(oid[:12] in comment for oid in ("2" * 40, "3" * 40, "4" * 40))


def test_a_single_file_that_times_out_unapplied_still_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    code, commits, comment = publish_split(tmp_path, monkeypatch, timeout_commit=1, sleeps=[])
    assert code == 1
    assert len(commits) == 1
    assert "No batch was confirmed" in comment


@pytest.mark.parametrize("lose_body", [False, True])
def test_a_committed_batch_whose_response_was_lost_is_reported_and_followed_by_the_remaining_batches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, lose_body: bool
) -> None:
    code, commits, comment = publish_split(tmp_path, monkeypatch, lose_response=1, lose_body=lose_body)
    assert code == 0
    assert [c["expectedHeadOid"] for c in commits] == ["a" * 40, "1" * 40, "2" * 40]
    assert all(oid[:12] in comment for oid in ("1" * 40, "2" * 40, "3" * 40))


@pytest.mark.parametrize(("delayed_reads", "success"), [(1, True), (2, True), (3, False)])
def test_a_gateway_timeout_waits_for_the_commit_to_become_visible(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, delayed_reads: int, success: bool
) -> None:
    sleeps: list[float] = []
    code, commits, comment = publish_split(
        tmp_path, monkeypatch, lose_response=1, delayed_reads=delayed_reads, sleeps=sleeps
    )
    assert sleeps == [60] * min(delayed_reads, 2)
    if success:
        assert code == 0
        assert [batch["expectedHeadOid"] for batch in commits] == ["a" * 40, "1" * 40, "2" * 40]
        assert "[111111111111]" in comment
    else:
        assert code == 1
        assert len(commits) == 1
        assert "No batch was confirmed" in comment


@pytest.mark.parametrize("wrong_blob", [False, True])
def test_a_lost_response_checks_an_unchanged_cassette_in_the_commit_tree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wrong_blob: bool
) -> None:
    code, commits, comment = publish_split(
        tmp_path, monkeypatch, lose_response=1, unchanged=True, wrong_blob=wrong_blob
    )
    assert len(commits[0]["fileChanges"]["additions"]) == 2
    if wrong_blob:
        assert code == 1
        assert len(commits) == 1
        assert "GitHub's commit result could not be verified" in comment
    else:
        assert code == 0
        assert commits[1]["expectedHeadOid"] == "1" * 40
        assert all(oid[:12] in comment for oid in ("1" * 40, "2" * 40))


def test_a_moved_branch_with_another_blob_is_not_treated_as_the_expected_batch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sleeps: list[float] = []
    code, commits, comment = publish_split(tmp_path, monkeypatch, lose_response=1, wrong_blob=True, sleeps=sleeps)
    assert sleeps == []
    assert code == 1
    assert len(commits) == 1
    assert "GitHub's commit result could not be verified" in comment
    assert "[111111111111]" not in comment


@pytest.mark.parametrize(
    ("retry", "unrelated", "head_replay", "unchanged"),
    [
        (True, False, None, False),
        (False, False, None, False),
        (True, True, None, False),
        (True, False, "cancelled", False),
        (True, False, "success", False),
        (True, False, None, True),
    ],
)
def test_manual_retry_only_reuses_ci_after_verified_cassette_commits(
    monkeypatch: pytest.MonkeyPatch, retry: bool, unrelated: bool, head_replay: str | None, unchanged: bool
) -> None:
    head = "b" * 40
    monkeypatch.setenv("REPO", REPO)
    monkeypatch.setenv("GH_TOKEN", "t")
    monkeypatch.setenv("INPUT_PR", "7")
    monkeypatch.setenv("RETRY", str(retry).lower())
    outputs = {}
    data = b'{"cassette":1,"task":"mock-basket"}\n'
    blob = hashlib.sha1(f"blob {len(data)}\0".encode() + data, usedforsecurity=False).hexdigest()

    def gh(*args: str, token: str, body: Any = None) -> Any:
        route = args[0]
        if route.endswith("/pulls/7"):
            repo = {"full_name": REPO}
            return {"state": "open", "head": {"sha": head, "repo": repo, "ref": "b"}, "base": {"repo": repo}}
        if "/actions/workflows/" in route:
            if f"head_sha={SHA}" in route:
                return [{"workflow_runs": [{"id": 7}]}]
            return [{"workflow_runs": [{"id": 8}] if head_replay else []}]
        if "/actions/runs/8/jobs" in route:
            return [{"jobs": [{"name": "replay", "conclusion": head_replay}]}]
        if "/actions/runs/7/jobs" in route:
            return [{"jobs": [{"name": "replay", "conclusion": "failure"}]}]
        if "/actions/runs/7/artifacts" in route:
            return {"artifacts": [{"name": "replay-misses", "expired": False}]}
        if route.endswith(f"/git/blobs/{blob}"):
            return {"encoding": "base64", "content": base64.encodebytes(data).decode()}
        if "/git/trees/" in route:
            return cassette_tree(route, "mock-basket.jsonl", blob)
        commit = {
            "message": f"{refill.REFILL_SUBJECT}mock-basket",
            "verification": {"verified": True},
            "tree": {"sha": "root"},
        }
        if route.endswith(f"/commits/{head}"):
            return {"commit": commit}
        if route.endswith(f"/commits/{head}?per_page=100"):
            files = (
                []
                if unchanged
                else [{"filename": "bench/cassettes/mock-basket.jsonl", "status": "modified", "sha": blob}]
            )
            if unrelated:
                files.append({"filename": "unrelated.py", "status": "modified"})
            return [{"commit": commit, "committer": {"login": "web-flow"}, "parents": [{"sha": SHA}], "files": files}]
        assert route.endswith("/issues/7/comments?per_page=100"), route
        return [
            [
                {
                    "user": {"login": refill.BOT},
                    "body": f"{refill.MARKER}\n<!-- cassette-refill-attempted: {SHA} -->",
                }
            ]
        ]

    def output(**values: str) -> None:
        outputs.update(values)

    monkeypatch.setattr(refill, "gh", gh)
    monkeypatch.setattr(refill, "output", output)
    if not retry or unrelated:
        with pytest.raises(SystemExit, match="no finished CI"):
            refill.resolve()
    else:
        assert refill.resolve() == 0
        expected = (
            {"run_id": "", "covered": "[]"}
            if head_replay == "success"
            else {"run_id": "7", "covered": '["mock-basket"]'}
        )
        assert outputs == {"pr": "7", "sha": head, **expected}


@pytest.mark.parametrize("middle_run", [None, ""])
def test_a_partial_retry_stops_at_an_ancestor_with_a_passing_replay(
    monkeypatch: pytest.MonkeyPatch, middle_run: str | None
) -> None:
    head, middle = "b" * 40, "c" * 40
    history = {head: (middle, "mock-basket"), middle: (SHA, "mock-swatch")}
    checked = []

    def read_commit(repo: str, sha: str, token: str) -> dict[str, Any]:
        parent, task = history[sha]
        return {
            "commit": {"message": f"{refill.REFILL_SUBJECT}{task}", "verification": {"verified": True}},
            "committer": {"login": "web-flow"},
            "parents": [{"sha": parent}],
            "files": [{"filename": f"bench/cassettes/{task}.jsonl", "status": "modified", "sha": "d" * 40}],
        }

    def replay_ci(repo: str, sha: str, token: str) -> str | None:
        checked.append(sha)
        return middle_run if sha == middle else "7"

    monkeypatch.setattr(refill, "pull_facts", lambda *args: (pull(head_sha=head), "b"))
    monkeypatch.setattr(refill, "find_comment", lambda *args: {"body": f"<!-- cassette-refill-attempted: {SHA} -->"})
    monkeypatch.setattr(refill, "read_commit", read_commit)
    monkeypatch.setattr(refill, "replay_ci", replay_ci)
    monkeypatch.setattr(refill, "verify_refill_blobs", lambda *args: None)
    resolved = refill.retry_from_partial(REPO, "7", head, "t")
    if middle_run is None:
        assert resolved == ("7", {"mock-basket", "mock-swatch"})
        assert checked == [middle, SHA]
    else:
        assert resolved == ("", set())
        assert checked == [middle]


def test_covered_task_blob_must_contain_a_valid_cassette(monkeypatch: pytest.MonkeyPatch) -> None:
    data = b'{"cassette":1,"task":"another-task"}\n'
    sha = hashlib.sha1(f"blob {len(data)}\0".encode() + data, usedforsecurity=False).hexdigest()

    def gh(*args: str, token: str, body: Any = None) -> Any:
        assert args == (f"repos/{REPO}/git/blobs/{sha}",)
        return {"encoding": "base64", "content": base64.encodebytes(data).decode()}

    monkeypatch.setattr(refill, "gh", gh)
    with pytest.raises(ValueError, match="no version 1 cassette head line"):
        refill.verify_refill_blobs(REPO, {"mock-basket": sha}, "t")


def test_missing_branch_facts_report_a_read_error(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(refill, "gh", lambda *args, **kwargs: {"head": None})
    with pytest.raises(ValueError, match="incomplete pull request facts"):
        refill.pull_facts(REPO, "7", "t")


def test_a_commit_file_list_at_the_rest_limit_cannot_verify_lineage(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(refill, "COMMIT_DIFF_FILE_LIMIT", 3)
    monkeypatch.setattr(refill, "gh", lambda *args, **kwargs: [{"files": [{}, {}]}, {"files": [{}]}])
    with pytest.raises(ValueError, match="prevents verifying all the changed files"):
        refill.read_commit(REPO, SHA, "t")


def test_batches_stay_below_the_commit_diff_file_limit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(refill, "COMMIT_DIFF_FILE_LIMIT", 3, raising=False)
    paths = [tmp_path / f"cassette-{n}.jsonl" for n in range(3)]
    for path in paths:
        path.write_bytes(b"x")
    assert refill.commit_batches(paths, 100) == [paths[:2], paths[2:]]


def test_a_cassette_over_the_request_limit_on_its_own_is_refused_by_name(tmp_path: Path) -> None:
    path = tmp_path / "mock-basket.jsonl"
    path.write_bytes(b"x" * 300)
    with pytest.raises(ValueError, match=r"mock-basket\.jsonl"):
        refill.commit_batches([path], limit=100)
