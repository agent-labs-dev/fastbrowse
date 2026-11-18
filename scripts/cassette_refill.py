"""The automatic cassette refill (`.github/workflows/cassette-refill.yml`): the decisions, and the steps that use them.

A pull request that changes what the agent asks makes the `replay` check miss. The check records each missed task as
a file (`record_misses`); the workflow reads them (`missed_tasks`), decides whether to spend anything
(`refill_decision`), fills only those tasks under a small cap, judges the result (`fill_verdict`), and commits the
cassettes only when every task is accepted (`validate_cassettes`).

The decisions use the standard library alone, so the jobs that hold the write token run without building the
package. Every step's input arrives in the environment, never in a command line, so a branch name or a title cannot
become code.

    resolve   which pull request, head commit and CI run the refill is about
    plan      the missed tasks, and whether to spend on them; claims the head commit before any spend
    fill      `bench fill --only` those tasks under the cap; runs the pull request's code and holds no write token
    publish   judge the fill, commit the cassettes to the branch if accepted, and tell the pull request either way
"""

import base64
import json
import math
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

TASK_ID = re.compile(r"[a-z0-9][a-z0-9-]*")
"""What a task id looks like. A misses file comes from the code of the pull request under test, so its names are
checked before they reach a path or a command line."""

CASSETTE_VERSION = 1
"""The cassette format `fastbrowse.evals.replay` reads. The jobs that judge a fill do not build the package, so they
keep their own copy, which a test holds equal to the package's."""

EXCHANGE_FIELDS: Mapping[str, type | tuple[type, ...]] = {
    "path": str,
    "purpose": str,
    "status": int,
    "content_type": str,
    "body": str,
    "latency": (int, float),
}
"""The typed fields `Exchange.from_json` requires of a cassette line, besides `request`, which may be any JSON."""

TRAILER = "Cassette-Refill"
"""The commit trailer that marks a refill commit, valued with the head commit it refilled."""

MARKER = "<!-- cassette-refill -->"
"""Identifies the one pull request comment the refill keeps up to date."""

_ATTEMPTED = re.compile(r"<!-- cassette-refill-attempted: ([0-9a-f,]*) -->")


# What the replay check found


def record_misses(directory: Path, task: str, misses: Sequence[Mapping[str, Any]]) -> None:
    """Write `<task>.json` for a task whose replay missed, so a later job reads the ids without parsing a log."""
    if not misses:
        return
    directory.mkdir(parents=True, exist_ok=True)
    listed = [{"purpose": miss["purpose"], "model": miss["model"]} for miss in misses]
    (directory / f"{task}.json").write_text(json.dumps({"task": task, "misses": listed}, indent=1) + "\n", "utf-8")


def missed_tasks(directory: Path) -> list[str]:
    """The tasks whose replay missed, sorted, from the files `record_misses` wrote. A directory that is absent holds
    none: the replay check failed for another reason, or passed."""
    if not directory.is_dir():
        return []
    tasks: set[str] = set()
    for path in sorted(directory.glob("*.json")):
        record = json.loads(path.read_text(encoding="utf-8"))
        task = record.get("task") if isinstance(record, dict) else None
        if not isinstance(task, str) or not TASK_ID.fullmatch(task) or path.name != f"{task}.json":
            raise ValueError(f"{path.name}: not a record of a missed task")
        if record.get("misses"):
            tasks.add(task)
    return sorted(tasks)


# Whether to refill


@dataclass(frozen=True)
class PullFacts:
    """What the refill reads about a pull request before it spends anything."""

    number: int
    open: bool
    head_sha: str
    head_repo: str
    base_repo: str
    head_message: str


@dataclass(frozen=True)
class Decision:
    run: bool
    reason: str


def attempted_shas(comment: str) -> list[str]:
    """The head commits the refill has already been tried for, from its comment's hidden record."""
    found = _ATTEMPTED.search(comment)
    return [sha for sha in found.group(1).split(",") if sha] if found else []


def is_refill_commit(message: str) -> bool:
    return any(line.startswith(f"{TRAILER}: ") for line in message.splitlines())


def refill_decision(
    pull: PullFacts, *, run_sha: str, tasks: Sequence[str], attempted: Iterable[str], retry: bool = False
) -> Decision:
    """Whether to spend on a refill of `tasks` for the check run on `run_sha`.

    The guards come first and each stops everything after it: a branch of a fork is never run with the model key, a
    closed or moved-on pull request is not worth the spend, and the refill neither refills its own commit nor tries
    one head commit twice, so it cannot loop. `retry` is a maintainer's explicit rerun and lifts only the last.
    """
    if pull.head_repo != pull.base_repo:
        return Decision(
            False, f"the pull request comes from a fork ({pull.head_repo}); its code is not run with the model key"
        )
    if not pull.open:
        return Decision(False, "the pull request is not open")
    if pull.head_sha != run_sha:
        return Decision(False, f"the check ran on {run_sha[:12]} but the branch is now at {pull.head_sha[:12]}")
    if is_refill_commit(pull.head_message):
        return Decision(False, "the branch head is itself a refill commit; a refill is never refilled")
    if not tasks:
        return Decision(False, "the replay check recorded no missed task")
    if not retry and pull.head_sha in set(attempted):
        return Decision(False, f"a refill was already tried for {pull.head_sha[:12]}")
    return Decision(True, f"refill {', '.join(tasks)}")


# Whether the fill worked


@dataclass(frozen=True)
class TaskResult:
    task: str
    cost: float | None
    reasons: tuple[str, ...]

    @property
    def passed(self) -> bool:
        return not self.reasons


@dataclass(frozen=True)
class FillVerdict:
    tasks: tuple[TaskResult, ...]
    spent: float
    budget: float
    reasons: tuple[str, ...]
    """What is wrong with the fill as a whole, apart from any one task."""
    complete: bool
    """Every requested task, and no other, was filled."""
    converged: bool
    """No task kept a miss or failed its grade."""
    unpriced: bool

    @property
    def accepted(self) -> bool:
        return not self.reasons and all(task.passed for task in self.tasks)


def positive(value: str) -> float:
    dollars = float(value)
    if not math.isfinite(dollars) or dollars <= 0:
        raise ValueError("a budget must be finite and positive")
    return dollars


def _spent(entry: Mapping[str, Any]) -> float | None:
    """What a task paid, a fill pass that was not kept included; a report without `spent_usd` has only what it kept."""
    spent = entry.get("spent_usd")
    return spent if spent is not None else entry.get("recorded_cost_usd")


def fill_verdict(report: Mapping[str, Any] | None, tasks: Sequence[str], budget: float, exit_code: int) -> FillVerdict:
    """Judge a `bench fill` run of `tasks`. It is accepted only when every task is complete, converged and graded
    passed, no exchange is unpriced, and the spend is within the budget."""
    entries = {entry["task"]: entry for entry in (report or {}).get("tasks", [])}
    results: list[TaskResult] = []
    for task in tasks:
        entry = entries.get(task)
        if entry is None:
            results.append(TaskResult(task, None, ("the fill never reached it (budget reached or an earlier error)",)))
            continue
        reasons: list[str] = []
        grade = entry["result"]["grade"]
        if not grade["passed"]:
            reasons.append(f"grade failed: {grade.get('failure') or 'no reason given'}")
        if entry["misses"]:
            reasons.append(f"{len(entry['misses'])} replay misses remained")
        if entry.get("upstream_errors"):
            reasons.append(f"upstream refused {len(entry['upstream_errors'])} requests {entry['upstream_errors']}")
        if entry.get("budget_refusals"):
            reasons.append(f"the budget stopped {entry['budget_refusals']} requests before they were sent")
        if entry.get("unpriced_exchanges"):
            reasons.append(f"{entry['unpriced_exchanges']} exchanges have no price")
        results.append(TaskResult(task, _spent(entry), tuple(reasons)))
    spent = sum(_spent(entry) or 0 for entry in entries.values())
    complete = set(entries) == set(tasks)
    reasons = []
    if report is None:
        reasons.append("the fill wrote no report")
    elif exit_code != 0:
        reasons.append(f"the fill exited {exit_code}")
    if report is not None and not complete and set(entries) - set(tasks):
        reasons.append(f"the fill ran tasks nobody asked for: {', '.join(sorted(set(entries) - set(tasks)))}")
    if spent > budget:
        reasons.append(f"over budget: ${spent:.4f} spent of ${budget:.2f}")
    return FillVerdict(
        tasks=tuple(results),
        spent=spent,
        budget=budget,
        reasons=tuple(reasons),
        complete=complete,
        converged=all(
            not entries[t]["misses"] and entries[t]["result"]["grade"]["passed"] for t in tasks if t in entries
        ),
        unpriced=any(entry.get("unpriced_exchanges") for entry in entries.values()),
    )


def validate_cassettes(directory: Path, tasks: Sequence[str]) -> list[Path]:
    """The cassette files to commit, or an error. Only `<task>.jsonl` of a requested task is taken, each must open
    with the head line of this cassette version for its task, every exchange must carry the fields replay reads, and
    none may hold an exchange the provider refused. The files come from a job that ran the pull request's code, so the
    committing job checks them itself."""
    found = sorted(path for path in directory.iterdir() if path.name != "result.json") if directory.is_dir() else []
    if any(path.is_symlink() or not path.is_file() for path in found):
        raise ValueError("a cassette to commit is not a regular file")
    wanted = {f"{task}.jsonl" for task in tasks}
    if {path.name for path in found} != wanted:
        raise ValueError(f"expected exactly {sorted(wanted)}, found {sorted(path.name for path in found)}")
    for path in found:
        with path.open(encoding="utf-8") as lines:
            head = json.loads(next(lines, "null"))
            if not isinstance(head, dict) or head.get("cassette") != CASSETTE_VERSION or head.get("task") != path.stem:
                raise ValueError(f"{path.name}: no version {CASSETTE_VERSION} cassette head line for its task")
            for number, line in enumerate(lines, 2):
                if not line.strip():
                    continue
                exchange = json.loads(line)
                if not isinstance(exchange, dict) or "request" not in exchange:
                    raise ValueError(f"{path.name}:{number}: not an exchange")
                wrong = sorted(
                    name
                    for name, kind in EXCHANGE_FIELDS.items()
                    if not isinstance(exchange.get(name), kind) or isinstance(exchange.get(name), bool)
                )
                if wrong:
                    raise ValueError(f"{path.name}:{number}: an exchange without a valid {', '.join(wrong)}")
                status = exchange["status"]
                if not 200 <= status < 300:
                    raise ValueError(f"{path.name}:{number}: a recorded status of {status!r} is not an answer")
    return found


# What the pull request is told


def commit_message(tasks: Sequence[str], head_sha: str, verdict: FillVerdict) -> str:
    listed = ", ".join(tasks)
    return (
        f"chore(bench): refill cassettes for {listed}\n\n"
        f"The replay check missed {len(tasks)} task(s) after the last change to what the agent asks. They were filled "
        f"with `bench fill` for ${verdict.spent:.4f} of a ${verdict.budget:.2f} cap, and each replays with no miss and "
        f"passes its grade.\n\n{TRAILER}: {head_sha}\n"
    )


def render_comment(
    *,
    head_sha: str,
    tasks: Sequence[str],
    verdict: FillVerdict | None,
    outcome: str,
    attempted: Iterable[str],
    run_url: str,
) -> str:
    """The pull request comment, also the job summary: what was asked, what each task did, and what came of it."""
    lines = [MARKER, f"<!-- cassette-refill-attempted: {','.join(dict.fromkeys(attempted))} -->", ""]
    lines += [f"### Cassette refill for `{head_sha[:12]}`", "", outcome, ""]
    if verdict is None:
        lines += [f"Tasks: {', '.join(f'`{task}`' for task in tasks)}", ""]
    else:
        lines += ["| task | result | cost | why |", "| --- | --- | --- | --- |"]
        for result in verdict.tasks:
            cost = "unknown" if result.cost is None else f"${result.cost:.4f}"
            state, why = ("passed" if result.passed else "failed"), "; ".join(result.reasons) or "-"
            lines.append(f"| `{result.task}` | {state} | {cost} | {why} |")
        lines += ["", f"Spent ${verdict.spent:.4f} of a ${verdict.budget:.2f} cap."]
        lines += [f"- {reason}" for reason in verdict.reasons]
        lines.append("")
    lines.append(f"[Workflow run]({run_url})")
    return "\n".join(lines) + "\n"


# The workflow's steps

BOT = "github-actions[bot]"
OUT = Path("artifacts/refill")
WORK = Path("artifacts/refill-work")
"""OUT is what the publishing job downloads; WORK holds bench's own report and logs, which are not uploaded."""
COMMIT = """
mutation($input: CreateCommitOnBranchInput!) {
  createCommitOnBranch(input: $input) { commit { oid url } }
}
"""


def gh(*args: str, token: str, body: Any = None) -> Any:
    """One GitHub REST or GraphQL call as `token`; the body, if any, goes in on stdin."""
    done = subprocess.run(
        ["gh", "api", *args, *(["--input", "-"] if body is not None else [])],
        input=json.dumps(body) if body is not None else None,
        capture_output=True,
        text=True,
        env={**os.environ, "GH_TOKEN": token},
        check=False,
    )
    if done.returncode != 0:
        raise RuntimeError(f"gh api {args[0]} failed: {done.stderr.strip()[:500]}")
    return json.loads(done.stdout) if done.stdout.strip() else None


def output(**values: str) -> None:
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.writelines(f"{key}={value}\n" for key, value in values.items())


def summary(text: str) -> None:
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
        stream.write(text + "\n")


def pull_facts(repo: str, number: str, token: str) -> tuple[PullFacts, str]:
    pull = gh(f"repos/{repo}/pulls/{number}", token=token)
    sha = pull["head"]["sha"]
    head_repo = (pull["head"]["repo"] or {}).get("full_name", "(deleted fork)")
    message = gh(f"repos/{repo}/commits/{sha}", token=token)["commit"]["message"]
    facts = PullFacts(
        number=int(number),
        open=pull["state"] == "open",
        head_sha=sha,
        head_repo=head_repo,
        base_repo=pull["base"]["repo"]["full_name"],
        head_message=message,
    )
    return facts, pull["head"]["ref"]


def find_comment(repo: str, number: str, token: str) -> dict[str, Any] | None:
    pages = gh(f"repos/{repo}/issues/{number}/comments?per_page=100", "--paginate", "--slurp", token=token)
    comments = [comment for page in pages for comment in page]
    return next((c for c in comments if MARKER in c["body"] and c["user"]["login"] == BOT), None)


def upsert_comment(repo: str, number: str, token: str, body: str) -> None:
    existing = find_comment(repo, number, token)
    if existing:
        gh(f"repos/{repo}/issues/comments/{existing['id']}", "-X", "PATCH", token=token, body={"body": body})
    else:
        gh(f"repos/{repo}/issues/{number}/comments", "-X", "POST", token=token, body={"body": body})


def resolve() -> int:
    env, repo, token = os.environ, os.environ["REPO"], os.environ["GH_TOKEN"]
    number = env["INPUT_PR"]
    if number:
        # A rerun by hand names the pull request; a check run names itself.
        sha = gh(f"repos/{repo}/pulls/{number}", token=token)["head"]["sha"]
        runs = gh(
            f"repos/{repo}/actions/workflows/ci.yml/runs?head_sha={sha}&event=pull_request&status=completed&per_page=1",
            token=token,
        )["workflow_runs"]
        if not runs:
            raise SystemExit(f"no finished CI run on {sha} of #{number}")
        output(pr=number, sha=sha, run_id=str(runs[0]["id"]))
        return 0
    output(pr=env["PR_NUMBER"], sha=env["RUN_SHA"], run_id=env["RUN_ID"])
    return 0


def plan() -> int:
    env, repo, token = os.environ, os.environ["REPO"], os.environ["GH_TOKEN"]
    number, sha = env["PR"], env["SHA"]
    if not number:
        output(run="false", tasks="[]")
        summary("No pull request is attached to this check run, so there is nothing to refill.")
        return 0
    tasks = missed_tasks(Path(env["MISSES_DIR"]))
    facts, _ = pull_facts(repo, number, token)
    existing = find_comment(repo, number, token)
    attempted = attempted_shas(existing["body"]) if existing else []
    decision = refill_decision(facts, run_sha=sha, tasks=tasks, attempted=attempted, retry=env["RETRY"] == "true")
    summary(f"Refill {'runs' if decision.run else 'does not run'}: {decision.reason}.")
    output(run=str(decision.run).lower(), tasks=json.dumps(tasks))
    if decision.run:
        # The head commit is claimed before any spend, so a crash or a rerun cannot buy the same refill twice.
        upsert_comment(
            repo,
            number,
            token,
            render_comment(
                head_sha=sha,
                tasks=tasks,
                verdict=None,
                outcome="Filling the missed tasks now.",
                attempted=[*attempted, sha],
                run_url=env["RUN_URL"],
            ),
        )
    return 0


def fill() -> int:
    tasks: list[str] = json.loads(os.environ["REFILL_TASKS"])
    budget = positive(os.environ["REFILL_BUDGET"])
    shutil.rmtree(OUT, ignore_errors=True)
    OUT.mkdir(parents=True)
    shutil.rmtree(WORK, ignore_errors=True)
    WORK.mkdir(parents=True)
    report_path = WORK / "fill.json"
    command = [
        sys.executable,
        "-m",
        "fastbrowse.evals.bench",
        "fill",
        # The agent the replay check runs, so a filled cassette answers exactly what that check asks.
        "--agent",
        "in-process",
        "--budget",
        str(budget),
        "--cassettes",
        str(Path("bench/cassettes").resolve()),
        "--out",
        str(report_path),
        "--only",
        *tasks,
    ]
    code = subprocess.run(command, check=False).returncode
    report = json.loads(report_path.read_text(encoding="utf-8")) if report_path.exists() else None
    (OUT / "result.json").write_text(json.dumps({"exit_code": code, "report": report}, indent=1) + "\n", "utf-8")
    for task in tasks:
        cassette = Path("bench/cassettes") / f"{task}.jsonl"
        if cassette.exists():
            shutil.copy(cassette, OUT / cassette.name)
    return 0


def publish() -> int:
    env, repo = os.environ, os.environ["REPO"]
    number, sha, token = env["PR"], env["SHA"], env["GH_TOKEN"]
    tasks: list[str] = json.loads(env["REFILL_TASKS"])
    budget = positive(env["REFILL_BUDGET"])
    artifact = Path(env["ARTIFACT_DIR"])
    result_path = artifact / "result.json"
    result = json.loads(result_path.read_text(encoding="utf-8")) if result_path.exists() else None
    verdict = fill_verdict(result["report"] if result else None, tasks, budget, result["exit_code"] if result else 1)
    committed = False
    if not result:
        outcome = "Nothing was committed: the fill job produced no result (see the workflow run)."
    elif not verdict.accepted:
        outcome = "Nothing was committed: the fill was rejected."
    else:
        try:
            files = validate_cassettes(artifact, tasks)
            facts, branch = pull_facts(repo, number, token)
            if not facts.open or facts.head_sha != sha or facts.head_repo != facts.base_repo:
                raise ValueError(f"the branch moved to {facts.head_sha[:12]} while the fill ran")
            if not env.get("COMMIT_TOKEN"):
                raise ValueError("the `AUR_SYNC_TOKEN` repository secret is not set, so no push could start CI again")
            headline, _, body = commit_message(tasks, sha, verdict).partition("\n\n")
            made = gh(
                "graphql",
                token=env["COMMIT_TOKEN"],
                body={
                    "query": COMMIT,
                    "variables": {
                        "input": {
                            "branch": {"repositoryNameWithOwner": repo, "branchName": branch},
                            "message": {"headline": headline, "body": body},
                            "expectedHeadOid": sha,
                            "fileChanges": {
                                "additions": [
                                    {
                                        "path": f"bench/cassettes/{path.name}",
                                        "contents": base64.b64encode(path.read_bytes()).decode(),
                                    }
                                    for path in files
                                ]
                            },
                        }
                    },
                },
            )
            if made.get("errors"):
                raise ValueError(f"GitHub refused the commit: {made['errors'][0]['message']}")
            commit = made["data"]["createCommitOnBranch"]["commit"]
            outcome = (
                f"Committed the refilled cassettes as [{commit['oid'][:12]}]({commit['url']}); CI runs again on it."
            )
            committed = True
        except (ValueError, RuntimeError) as error:
            outcome = f"Nothing was committed: {error}"
    existing = find_comment(repo, number, token)
    attempted = attempted_shas(existing["body"]) if existing else [sha]
    body = render_comment(
        head_sha=sha, tasks=tasks, verdict=verdict, outcome=outcome, attempted=attempted, run_url=env["RUN_URL"]
    )
    upsert_comment(repo, number, token, body)
    summary(body)
    return 0 if committed else 1


def main() -> int:
    modes = {"resolve": resolve, "plan": plan, "fill": fill, "publish": publish}
    if len(sys.argv) != 2 or sys.argv[1] not in modes:
        raise SystemExit(f"usage: cassette_refill.py {{{','.join(modes)}}}")
    return modes[sys.argv[1]]()


if __name__ == "__main__":
    raise SystemExit(main())
