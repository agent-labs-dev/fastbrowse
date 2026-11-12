import asyncio
import itertools
import json
import shutil
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
from fastbrowse.models import Limits

from fastbrowse_evals import live, official, official_rows, publication
from fastbrowse_evals.attempt_clock import SESSION_OPEN_SECONDS, TIMING_BASIS
from fastbrowse_evals.catalog import render
from fastbrowse_evals.judging import OPENROUTER_BASE_URL, OPUS_JUDGE
from fastbrowse_evals.live import AGENT_LIMITS, JUDGE_POLICY
from fastbrowse_evals.publication import Candidate, Checks, OfficialSource, PublicationError

AGENT, RUNNER = "a" * 40, "b" * 40
CATALOG = json.loads(render())
DIGEST = __import__("hashlib").sha256(render().encode()).hexdigest()
UNCAPPED = AGENT_LIMITS.model_dump(mode="json")
LOCAL_CHROME = {
    "mode": "local-chrome",
    "binary": "/test/chrome",
    "version": "Chromium 123",
    "sha256": "e" * 64,
    "headed": False,
    "profile": False,
}
BROWSERS = {
    "fastbrowse": {"mode": "browser-use-cloud", "timeout_minutes": 240},
    "fastbrowse-pool": {"mode": "fastbrowse-pool", "host": "pool.internal"},
    "jev-ultrafast": {"mode": "browser-use-cloud", "timeout_minutes": 240},
    "browser-use": {"mode": "browser-use-hosted"},
    "browser-use-ultrafast": {"mode": "browser-use-hosted"},
    "browser-harness": {"mode": "browser-use-cloud", "timeout_minutes": 240},
}


def entries(arms: tuple[str, ...]) -> list[dict[str, Any]]:
    return [
        t | {"suite": name} for name, suite in CATALOG["suites"].items() for t in suite if set(arms) <= set(t["arms"])
    ]


def run_record(arms: tuple[str, ...], **over: Any) -> dict[str, Any]:
    """One invocation's provenance under the uncapped start-gate protocol."""
    record = {
        "run_id": "r1",
        "git_sha": AGENT,
        "git_dirty": False,
        "fastbrowse_version": "1.0",
        "benchmark_family": "browser-use",
        "benchmark_origin": "fastbrowse-internal",
        "benchmark_runner_sha": RUNNER,
        "benchmark_runner_dirty": False,
        "benchmark_catalog_sha256": DIGEST,
        "arms": {a: {"pin": CATALOG["arms"][a]["pin"], "tier": CATALOG["arms"][a]["tier"]} for a in arms},
        "budget_policy": "start-gate-v1",
        "budget_usd": 100.0,
        "reserve_dollars": 1.0,
        "browser_reserve_dollars": 1.0,
        "judge_reserve_dollars": 1.0,
        "max_steps": None,
        "max_seconds": None,
        "session_open_limit_seconds": SESSION_OPEN_SECONDS,
        "timing_basis": TIMING_BASIS,
        "max_dollars": None,
        "agent_limits": UNCAPPED,
        "hosted_max_cost_usd": "1000" if live.HOSTED_MODELS.keys() & set(arms) else None,
        "browsers": {a: BROWSERS[a] for a in arms},
        "concurrency": 8,
        "run_started": "2026-10-07T09:00:00+00:00",
    }
    return record | over


def semantic_receipt(passed: bool, judge_cost: float = 0) -> dict[str, Any]:
    return {
        "policy_sha256": "f" * 64,
        "judgment": {
            "policy": JUDGE_POLICY,
            "state_sha256": "e" * 64,
            "questions": {},
            "fallback_calls": (
                [
                    {
                        "model": "independent-judge",
                        "attempt": 1,
                        "cost_usd": judge_cost,
                        "latency_ms": 1,
                    }
                ]
                if judge_cost
                else []
            ),
            "decisions": {
                "evidence_available": {"passed": True, "source": "llm", "reason": "Recorded execution"},
                "task_satisfied": {"passed": passed, "source": "llm", "reason": "Recorded judgment"},
            },
        },
    }


def judge_spend(attempt: dict[str, Any]) -> float:
    """What an attempt's semantic judge charged: every priced fallback call in its receipt."""
    calls = (attempt.get("semantic_judge") or {}).get("judgment", {}).get("fallback_calls", [])
    return sum(call["cost_usd"] for call in calls)


def write_journal(results: Path, attempts: list[dict[str, Any]], run: dict[str, Any]) -> None:
    """The start-gate journal a serial run of `attempts` writes: each run's hold (agent, browser and judge reserves),
    then its actual charge (agent plus judge spend)."""
    hold = run["reserve_dollars"] + run["browser_reserve_dollars"] + run["judge_reserve_dollars"]
    journal = []
    for a in attempts:
        common = {"id": f"{a['arm']}/{a['task']}/{a['repeat']}/{a['retries']}", "limit": run["budget_usd"]}
        common |= {"reservation": hold, "policy": "start-gate-v1"}
        # An unknown charge keeps its whole hold, as `CampaignBudget.settle` does.
        charge = hold if a.get("unknown_cost") or a.get("dollars") is None else a["dollars"] + judge_spend(a)
        journal += [common | {"committed": hold}, common | {"committed": charge}]
    results.with_suffix(".budget.jsonl").write_text("".join(json.dumps(r) + "\n" for r in journal))


def write_ledger(
    folder: Path,
    name: str,
    arms: tuple[str, ...],
    edit: Callable[[list[dict[str, Any]]], None] | None = None,
    task_arms: tuple[str, ...] | None = None,
    **run_over: Any,
) -> Path:
    """A ledger of `arms` on the tasks every one of `task_arms` (default `arms`) runs, each arm on those it supports."""
    run = run_record(arms, **run_over)
    fastbrowse_only = set(arms) <= publication.FASTBROWSE_ARMS
    attempts = []
    # A hosted head-to-head covers fastbrowse's tasks; each other pair the tasks both of its arms run.
    hosted = "fastbrowse" in arms and not fastbrowse_only
    for t in entries(task_arms or (("fastbrowse",) if hosted else arms)):
        for arm in (a for a in arms if a in t["arms"]):
            for repeat in range(3):
                attempts.append(
                    {
                        "arm": arm,
                        "task": t["id"],
                        "repeat": repeat,
                        "retries": 0,
                        "selected": True,
                        "status": "complete" if arm in publication.FASTBROWSE_ARMS else "finished",
                        "normalized_status": "done",
                        "passed": repeat != 1,
                        "semantic_judge": semantic_receipt(repeat != 1),
                        "seconds": 2.0,
                        "timing_basis": TIMING_BASIS,
                        "dollars": 0.25 if not fastbrowse_only else 0.01,
                        "unknown_cost": False,
                        "task_version": t["task_version"],
                        "suite": t["suite"],
                        "concurrency": run["concurrency"],
                        "model": "jev-model" if fastbrowse_only else "model-x" if arm != "fastbrowse" else None,
                        "text_model": "google/planner-model, google/reader-model" if fastbrowse_only else None,
                        "run": run,
                    }
                )
    if edit:
        edit(attempts)
    results = folder / f"{name}.jsonl"
    results.write_text("".join(json.dumps(a) + "\n" for a in attempts if a["selected"]))
    results.with_suffix(".attempts.jsonl").write_text("".join(json.dumps(a) + "\n" for a in attempts))
    write_journal(results, attempts, run)
    return results


def write_official(
    folder: Path,
    suite: str,
    count: int,
    edit: Callable[[dict[str, Any]], None] | None = None,
    repeats: int = 1,
) -> tuple[Path, list[str]]:
    ids = [f"{suite[:2]}{i}" for i in range(count)]
    config: dict[str, Any] = {
        "suite": suite,
        "repeats": repeats,
        "arm": "fastbrowse",
        "upstream_commit": "c" * 40,
        "dataset_sha256": "d" * 64,
        "judge_model": OPUS_JUDGE,
        "judge_base_url": OPENROUTER_BASE_URL,
        "judge_reasoning": "medium",
        "score_threshold": 3,
        "judge_versions": {"openai": "1.0"},
        "judge_billing": {"budget_usd": 0.5, "input_usd_per_mtok": 100.0, "output_usd_per_mtok": 1000.0},
        "judge_pricing_checked": "2026-10-01",
        "screenshot_timing": official.FRAME_TIMING[suite],
        "budget_policy": "start-gate-v1",
        "concurrency": 1,
        "agent_limits": UNCAPPED,
        "max_steps": None,
        "task_timeout": None,
        "session_open_limit_seconds": SESSION_OPEN_SECONDS,
        "timing_basis": TIMING_BASIS,
        "agent_budget": None,
        "reserve_dollars": 1.0,
        "browser_reserve_dollars": 0.0,
        "agent_runtime": {"browser": {"mode": "local-chrome"}},
        "authorize_irreversible_actions": False,
        "task_ids": ids,
    }
    manifest = {
        **config,
        "config_sha256": official.config_fingerprint(config),
        "total_budget": 100.0,
        "published": False,
        "run": {
            "git_sha": AGENT,
            "git_dirty": False,
            "benchmark_runner_sha": RUNNER,
            "benchmark_runner_dirty": False,
            "benchmark_catalog_sha256": DIGEST,
            "benchmark_origin": "upstream",
            "benchmark_suite": suite,
            "run_started": "2026-10-07T09:00:00+00:00",
        },
    }
    if edit:
        edit(manifest)
    run = folder / suite
    run.mkdir()
    (run / "manifest.json").write_text(json.dumps(manifest))
    results, attempts = [], []
    for (i, task), repeat in itertools.product(enumerate(ids), range(repeats)):
        score = 0.5 if suite == "bu-bench" and (i + repeat) % 2 else float((i + repeat) % 2)
        results.append(
            {
                "task": task, "repeat": repeat, "arm": "fastbrowse", "status": "complete", "completed": True,
                "agent_dollars": 0.1, "agent_cost_unknown": False, "seconds": 3.0, "timing_basis": TIMING_BASIS,
                "upstream_score": score, "upstream_passed": bool(score), "judge_usd": 0.02,
                "final_capture_available": False, "agent_limits": UNCAPPED,
            }
        )  # fmt: skip
        attempts.append(
            {"task": task, "repeat": repeat, "started": True, "reserved_usd": 1.5, "agent_limits": UNCAPPED}
        )
        artifact = run / f"{task}-{repeat}"
        artifact.mkdir()
        (artifact / "agent.json").write_text(
            json.dumps(
                {
                    "status": "complete",
                    "answer": None,
                    "data": None,
                    "evidence": [],
                    "steps": [],
                    "artifacts": [],
                    "cost": {"lines": [{"component": "llm", "basis": "metered", "dollars": 0.1}]},
                }
            )
        )
        (artifact / "timing.json").write_text(json.dumps({"agent_seconds": 3.0, "timing_basis": TIMING_BASIS}))
        (artifact / "grade.json").write_text(json.dumps({"score": score, "passed": bool(score)}))
        receipt = [
            {"event": "reserved", "n": 1, "reserved_usd": 0.05},
            {"event": "settled", "n": 1, "usd": 0.02, "input_tokens": 100, "output_tokens": 10},
        ]
        (artifact / "receipts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in receipt))
    (run / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in results))
    (run / "attempts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in attempts))
    write_official_journal(run, [[f"{t}-{r}"] for t in ids for r in range(repeats)], manifest["total_budget"])
    return run, ids


def write_official_journal(
    run: Path, waves: list[list[str]], limit: Any, hold: float = 1.5, charge: float = 0.12
) -> None:
    """The start gate's journal for `waves` of attempts: each wave's holds taken together, then each settled."""
    receipts = []
    for wave in waves:
        receipts += [official.journal_row(run_id, limit, hold, hold) for run_id in wave]
        receipts += [official.journal_row(run_id, limit, hold, charge) for run_id in wave]
    (run / official.JOURNAL).write_text("".join(json.dumps(r) + "\n" for r in receipts))


def refit(**change: Any) -> Callable[[dict[str, Any]], None]:
    """A manifest edit that keeps the recorded configuration fingerprint honest, so only the edit is under test."""

    def edit(manifest: dict[str, Any]) -> None:
        manifest.update(change)
        config = {k: v for k, v in manifest.items() if k not in publication.CONFIG_EXTRAS}
        manifest["config_sha256"] = official.config_fingerprint(config)

    return edit


class Harness:
    def __init__(self, tmp_path: Path, **edits: Any) -> None:
        self.tmp = tmp_path
        self.catalog = tmp_path / "benchmark-catalog.json"
        self.catalog.write_text(render())
        self.hosted = write_ledger(
            tmp_path,
            "hosted",
            ("fastbrowse", "browser-use", "browser-use-ultrafast", "browser-harness"),
            edits.get("hosted"),
            **{"budget_usd": 1000.0} | edits.get("hosted_run", {}),
        )
        self.fast = write_ledger(tmp_path, "ultra", ("fastbrowse-pool", "jev-ultrafast"), edits.get("ultra"))
        self.bu, bu_ids = write_official(tmp_path, "bu-bench", 200, edits.get("bu"))
        self.m2w, m2w_ids = write_official(tmp_path, "online-mind2web", 300, edits.get("m2w"))
        self.ids = {"bu-bench": bu_ids, "online-mind2web": m2w_ids}
        self.read: list[Any] = []
        self.fixtures: list[tuple[int, str]] = []

    async def load_tasks(self, args: Any) -> list[dict[str, Any]]:
        return [{"task_id": t} for t in self.ids[args.suite.value]]

    async def grades(self, args: Any, records: dict[str, dict[str, str]]) -> dict[str, Any]:
        return {key: json.loads((Path(r["folder"]) / "grade.json").read_text()) for key, r in records.items()}

    def export(self, **over: Any) -> Candidate:
        readback = over.pop("readback", self.read.append)
        checks = Checks(
            readback=readback,
            official_readback=lambda part: None,
            fixture=lambda candidate: self.fixtures.append((candidate.fixture.run_id, candidate.agent_sha)),
            runner=lambda sha: None,
        )
        rows = []
        for group, source in (
            ("bu-bench", over.pop("bu_bench", OfficialSource(self.bu, self.tmp / "bu.enc"))),
            ("online-mind2web", over.pop("mind2web", OfficialSource(self.m2w, self.tmp / "m2w.json"))),
        ):
            if source is not None:
                part, tracked = asyncio.run(publication.official_group(group, source, self.load_tasks, self.grades))
                readback(tracked)
                rows.append(part)
        return publication.export(
            campaign_id="candidate-1",
            catalog=self.catalog,
            hosted=over.pop("hosted", self.hosted),
            ultrafast=over.pop("ultrafast", self.fast),
            official_rows=over.pop("official_rows", rows),
            fixture_run=77,
            checks=checks,
            **over,
        )


def test_complete_official_suite_does_not_require_comparator_reruns(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    candidate = h.export(hosted=None, ultrafast=None, mind2web=None)
    assert candidate.schema_version == publication.timed_schema(2)
    assert [g.id for g in candidate.groups] == ["bu-bench"]
    assert len(candidate.groups[0].rows) == 200
    assert len(h.read) == 1 and h.fixtures == [(77, AGENT)]


def test_publication_help_reaches_the_suite_parser(capsys: pytest.CaptureFixture[str]) -> None:
    from fastbrowse_evals.cli import main

    with pytest.raises(SystemExit) as exit:
        main(["browser", "publish-candidate", "--help"])
    assert exit.value.code == 0
    assert "--official-rows" in capsys.readouterr().out


def test_export_refuses_an_empty_campaign(tmp_path: Path) -> None:
    with pytest.raises(PublicationError, match="at least one complete"):
        Harness(tmp_path).export(hosted=None, ultrafast=None, bu_bench=None, mind2web=None)


def test_official_source_does_not_depend_on_internal_catalog_revisions(tmp_path: Path) -> None:
    h = Harness(tmp_path, bu=lambda m: m["run"].update(benchmark_catalog_sha256="0" * 64))
    candidate = h.export(hosted=None, ultrafast=None, mind2web=None)
    assert candidate.groups[0].source_sha256 == "d" * 64
    assert candidate.catalog_sha256 == DIGEST


def test_export_counts_every_physical_run_and_keeps_source_grades_separate(tmp_path: Path) -> None:
    def retry_one(attempts: list[dict[str, Any]]) -> None:
        first = next(a for a in attempts if a["arm"] == "browser-use")
        outage = dict(first, selected=False, status="unavailable", normalized_status="unavailable")
        outage |= {"seconds": 5.0, "dollars": 0.5, "passed": False}
        outage["llm_calls"], first["llm_calls"] = 2, 3
        outage["semantic_judge"] = json.loads(json.dumps(first["semantic_judge"]))
        outage["semantic_judge"]["judgment"]["decisions"]["task_satisfied"]["passed"] = False
        first["retries"] = 1
        attempts.insert(attempts.index(first), outage)

    h = Harness(tmp_path, hosted=retry_one)
    candidate = h.export()
    groups = {g.id: g for g in candidate.groups}
    assert set(groups) == {"internal-hosted", "internal-ultrafast", "bu-bench", "online-mind2web"}
    assert (candidate.agent_sha, candidate.runner_sha, candidate.catalog_sha256) == (AGENT, RUNNER, DIGEST)
    assert h.fixtures == [(77, AGENT)] and len(h.read) == 4
    retried = next(r for r in groups["internal-hosted"].rows if r.physical_runs == 2)
    assert (retried.seconds, retried.dollars, retried.score, retried.completed) == (7.0, 0.75, 1.0, True)
    assert retried.llm_calls == 5
    assert sum(r.physical_runs for r in groups["internal-hosted"].rows) == (53 + 52 + 52 + 47) * 3 + 1
    # internal-ultrafast is fastbrowse on its own pool against Ultrafast, which only schema 5 admits.
    assert candidate.schema_version == publication.timed_schema(5)
    assert {r.arm for r in groups["internal-ultrafast"].rows} == {"fastbrowse-pool", "jev-ultrafast"}
    bu = groups["bu-bench"]
    assert {r.score for r in bu.rows} == {0.0, 0.5} and all(r.completed for r in bu.rows)
    assert bu.rows[0].dollars == pytest.approx(0.1) and bu.source_sha256 == "d" * 64
    assert bu.rows[0].llm_calls is None
    assert bu.rows[0].llm_calls_basis == "physical_requests"
    assert groups["internal-hosted"].source_sha256 == DIGEST
    assert 1 <= len(bu.limits) <= 12
    assert any("Judge anthropic claude-opus-5.5 via OpenRouter, reasoning medium" in n for n in bu.limits)
    assert any("model-x" in n for n in groups["internal-hosted"].limits)
    # the closed public shape survives a round trip through its own strict contract
    Candidate.model_validate_json(json.dumps(candidate.model_dump(mode="json")))
    # The efficiency figures are part of the pinned contract's row (fastbrowse v0.7.1): an export that does not
    # record them carries the contract's own zero, which the site reads as not recorded.
    assert set(json.loads(candidate.model_dump_json())["groups"][0]["rows"][0]) - {"timing_basis"} == {
        "task", "repeat", "arm", "status", "completed", "score", "seconds", "dollars", "physical_runs",
        "llm_calls", "jev_calls", "tokens_per_second", "input_tokens", "output_tokens",
    }  # fmt: skip


@pytest.mark.parametrize(
    ("edits", "message"),
    [
        ({"hosted": lambda a: a[0].update(dollars=None)}, "dollars"),
        ({"hosted": lambda a: a[0].update(unknown_cost="unknown")}, "unknown_cost"),
        ({"hosted": lambda a: a[0].pop("seconds")}, "seconds"),
        ({"hosted": lambda a: a[0].update(passed=None)}, "judgment"),
        ({"hosted": lambda a: a.pop()}, "every task"),
        ({"hosted": lambda a: a[0].update(task_version=999)}, "task version"),
        ({"hosted": lambda a: a[0].update(concurrency=1)}, "concurrency"),
        ({"hosted_run": {"budget_usd": None}}, "budget"),
        ({"hosted_run": {"max_seconds": 3600}}, "limit was recorded"),
        ({"hosted_run": {"max_steps": 50}}, "limit was recorded"),
        ({"hosted_run": {"agent_limits": UNCAPPED | {"max_dollars": 1.0}}}, "limit was recorded"),
        ({"hosted_run": {"hosted_max_cost_usd": None}}, "max_cost_usd"),
        ({"hosted_run": {"budget_policy": "reserve-next-task-v1"}}, "start-gate-v1"),
        ({"hosted_run": {"browsers": {}}}, "browser"),
        ({"hosted_run": {"git_dirty": True}}, "clean"),
        ({"hosted_run": {"benchmark_origin": "upstream"}}, "canonical internal catalog"),
        ({"ultra": lambda a: a[0].update(selected=False)}, "final attempt|one row per slot"),
        ({"ultra": lambda a: a[0].update(retries=2)}, "retry sequence"),
        ({"bu": lambda m: m["run"].update(git_sha="e" * 40)}, "one clean agent build"),
        ({"bu": lambda m: m["run"].update(benchmark_runner_dirty=True)}, "clean"),
        ({"bu": refit(judge_versions={})}, "judge versions"),
        ({"bu": lambda m: m.update(config_sha256="9" * 64)}, "fingerprint"),
        ({"m2w": lambda m: m.update(published=True)}, "private Mind2Web run"),
        ({"m2w": lambda m: m["task_ids"].reverse()}, "fingerprint|task source"),
        ({"bu": refit(judge_model="openai/gpt-5")}, "OpenRouter alone"),
        ({"bu": refit(judge_base_url="https://api.openai.com/v1")}, "OpenRouter alone"),
        ({"bu": refit(arm="browser-use")}, "browser-use cannot be published"),
        ({"bu": refit(repeats=0)}, "at least one repeat"),
    ],
)
def test_incomplete_unknown_or_mismatched_evidence_blocks_export(
    tmp_path: Path, edits: dict[str, Any], message: str
) -> None:
    h = Harness(tmp_path, **edits)
    with pytest.raises((PublicationError, ValueError), match=message):
        h.export()


def test_manifest_task_ids_must_equal_the_actual_source(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    h.ids["online-mind2web"] = [*h.ids["online-mind2web"][:-1], "other"]
    with pytest.raises(PublicationError, match="actual task source"):
        h.export()


def test_official_results_need_known_timing_agent_charge_and_settled_judge_charge(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    path = h.bu / "results.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]

    def rewrite(index: int, **change: Any) -> None:
        mutated = [dict(r) for r in rows]
        mutated[index] |= change
        path.write_text("".join(json.dumps(r) + "\n" for r in mutated))

    for change, message in [
        ({"seconds": None}, "saved artifacts"),
        ({"agent_cost_unknown": True}, "agent charge is unknown"),
        ({"upstream_score": None}, "saved judge response"),
        ({"judge_usd": 0.5}, "private receipts"),
    ]:
        rewrite(0, **change)
        with pytest.raises(PublicationError, match=message):
            h.export()
    rewrite(0)
    (h.bu / "bu0-0" / "receipts.jsonl").write_text(json.dumps({"event": "reserved", "n": 1, "reserved_usd": 0.1}))
    with pytest.raises(PublicationError, match="unsettled"):
        h.export()


def test_mind2web_cannot_publish_a_partial_grade(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    path = h.m2w / "results.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    rows[0]["upstream_score"] = 0.5
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    with pytest.raises(ValueError, match="saved judge response"):
        h.export()


def test_noncanonical_catalog_bytes_and_failed_readback_publish_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    h = Harness(tmp_path)
    h.catalog.write_text(render().rstrip("\n"))
    with pytest.raises(PublicationError, match="canonical export"):
        h.export()
    h.catalog.write_text(render())
    # A gate may measure a build older than these tasks; such a run is never published.
    with monkeypatch.context() as m:
        m.setattr(publication, "installed_catalog", lambda: b"an older build's catalog")
        with pytest.raises(PublicationError, match="different benchmark catalog"):
            h.export()

    def missing(_: Any) -> None:
        raise PublicationError("not fully uploaded")

    with pytest.raises(PublicationError, match="not fully uploaded"):
        h.export(readback=missing)


def test_limits_are_short_ascii_phrases_without_links() -> None:
    assert publication.note("Judge vendor/model-1.5") == "Judge vendor:model-1.5"
    for bad in ("see example.com", "visit https:x", "naïve", "x" * 241, ""):
        with pytest.raises(PublicationError):
            publication.note(bad)


# --- Langfuse readback against a fake of the experiments API ---


def fake_langfuse(tracked: Any, *, drop: str | None = None, tamper: bool = False) -> Any:
    runs = [r for r in tracked.runs if r.name != drop]

    def item(trace: Any) -> Any:
        output = trace.output.model_dump(mode="json")
        if tamper:
            output["repeats"] = output["repeats"][:-1]
        return SimpleNamespace(
            experiment_item_id=trace.item_id,
            trace_id="t",
            id="o",
            input=json.dumps(trace.input.model_dump(mode="json")),
            output=json.dumps(output),
            scores=[SimpleNamespace(name=s.name) for s in trace.scores],
        )

    def page(data: list[Any]) -> Any:
        return SimpleNamespace(data=data, meta=SimpleNamespace(cursor=None))

    def list_runs(**kw: Any) -> Any:
        found = [r for r in runs if kw.get("id") in (None, r.name)]
        return page(
            [
                SimpleNamespace(name=r.name, id=r.name, scores=[SimpleNamespace(name=s.name) for s in r.scores])
                for r in found
            ]
        )

    def list_items(**kw: Any) -> Any:
        run = next(r for r in runs if r.name == kw["experiment_id"])
        return page([item(t) for t in run.traces])

    return SimpleNamespace(
        get_dataset=lambda name: SimpleNamespace(id="d1"),
        api=SimpleNamespace(experiments=SimpleNamespace(list=list_runs, list_items=list_items)),
    )


def tracked_official(tmp_path: Path) -> Any:
    h = Harness(tmp_path)
    seen: list[Any] = []
    h.export(readback=seen.append)
    return seen[2]


def test_readback_requires_every_experiment_item_and_physical_run(tmp_path: Path) -> None:
    tracked = tracked_official(tmp_path)
    project = SimpleNamespace(name="fastbrowse-evals")
    publication.confirm_langfuse(fake_langfuse(tracked), project, tracked)
    with pytest.raises(PublicationError, match="not fully uploaded"):
        publication.confirm_langfuse(fake_langfuse(tracked, drop=tracked.runs[0].name), project, tracked)
    with pytest.raises(PublicationError, match="exact source rows"):
        publication.confirm_langfuse(fake_langfuse(tracked, tamper=True), project, tracked)
    with pytest.raises(PublicationError, match="fastbrowse-evals"):
        publication.confirm_langfuse(fake_langfuse(tracked), SimpleNamespace(name="production"), tracked)


def test_a_conservative_judge_reservation_is_not_a_reconciled_cost(tmp_path: Path) -> None:
    folder = tmp_path / "task"
    folder.mkdir()
    receipt = [
        {"event": "reserved", "n": 1, "reserved_usd": 0.5},
        {"event": "settled", "n": 1, "usd": 0.5, "input_tokens": None, "output_tokens": None},
    ]
    (folder / "receipts.jsonl").write_text("".join(json.dumps(row) + "\n" for row in receipt))
    with pytest.raises(PublicationError, match="reconciled usage"):
        publication.settled_judge_usd(folder, {"input_usd_per_mtok": 1, "output_usd_per_mtok": 1})


def test_publication_reconciles_context_tier_and_rejects_undercharged_receipt(tmp_path: Path) -> None:
    billing = {
        "input_usd_per_mtok": 1,
        "output_usd_per_mtok": 2,
        "context_threshold_tokens": 1000,
        "context_input_usd_per_mtok": 2,
        "context_output_usd_per_mtok": 3,
    }
    receipt = [
        {"event": "reserved", "n": 1, "reserved_usd": 0.01},
        {"event": "settled", "n": 1, "usd": 0.002302, "input_tokens": 1001, "output_tokens": 100},
    ]
    path = tmp_path / "receipts.jsonl"
    path.write_text("".join(json.dumps(row) + "\n" for row in receipt))
    assert publication.settled_judge_usd(tmp_path, billing) == pytest.approx(0.002302)
    receipt[1]["usd"] = 0.001201
    path.write_text("".join(json.dumps(row) + "\n" for row in receipt))
    with pytest.raises(PublicationError, match="differs from its token receipt"):
        publication.settled_judge_usd(tmp_path, billing)


@pytest.mark.parametrize(
    "change",
    [{"seconds": 1.0}, {"agent_dollars": 0.0}, {"status": "error"}, {"completed": False}, {"upstream_score": 1.0}],
)
def test_stale_official_rows_cannot_override_saved_artifacts(tmp_path: Path, change: dict[str, Any]) -> None:
    h = Harness(tmp_path)
    path = h.bu / "results.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    rows[0].update(change)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    with pytest.raises(PublicationError, match="saved artifacts|saved judge response"):
        h.export()


def test_judge_dollars_must_equal_token_receipts(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    path = h.bu / "bu0-0" / "receipts.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    rows[1]["usd"] = 0.0
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    with pytest.raises(PublicationError, match="token receipt"):
        h.export()


def arm_rows(h: Harness, source: OfficialSource) -> publication.OfficialRows:
    return asyncio.run(publication.official_group("bu-bench", source, h.load_tasks, h.grades))[0]


def test_an_official_group_holds_fastbrowse_alone(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    fast = arm_rows(h, OfficialSource(h.bu, h.tmp / "bu.enc"))
    kwargs: dict[str, Any] = {"hosted": None, "ultrafast": None, "bu_bench": None, "mind2web": None}
    group = h.export(official_rows=[fast], **kwargs).groups[0]
    assert {r.arm for r in group.rows} == {"fastbrowse"} and len(group.rows) == 200
    assert any(n.startswith("Browser: ") for n in group.limits)
    with pytest.raises(PublicationError, match="two official runs"):
        h.export(official_rows=[fast, fast], **kwargs)


def test_a_campaign_ledger_is_tracked_for_every_arm(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    from fastbrowse_evals import track_ledger

    parallax_rows(monkeypatch)
    h = Harness(tmp_path)
    track_ledger.main([str(h.hosted)])
    experiments = json.loads(capsys.readouterr().out)["experiments"]
    assert [name.split("/")[1] for name in experiments] == [
        "fastbrowse",
        "browser-use",
        "browser-use-ultrafast",
        "browser-harness",
    ]


def test_official_rows_hold_nothing_but_the_public_fields(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    part = arm_rows(h, OfficialSource(h.bu, h.tmp / "bu.enc"))
    assert set(part.model_dump()) == set(publication.OfficialRows.model_fields)
    with pytest.raises(ValueError, match="Extra inputs"):
        publication.OfficialRows.model_validate({**part.model_dump(), "goal": "a decrypted goal"})


def test_the_langfuse_readback_covers_official_rows(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    part = arm_rows(h, OfficialSource(h.bu, h.tmp / "bu.enc"))
    seen: list[str] = []
    checks = Checks(
        readback=lambda tracked: None,
        official_readback=lambda rows: seen.append(rows.langfuse_run),
        fixture=lambda candidate: None,
        runner=lambda sha: None,
    )
    publication.export(
        campaign_id="candidate-1", catalog=h.catalog, hosted=None, ultrafast=None, official_rows=[part],
        fixture_run=77, checks=checks,
    )  # fmt: skip
    assert seen == [part.langfuse_run]


def test_bu_publication_discloses_missing_final_captures(tmp_path: Path) -> None:
    candidate = Harness(tmp_path).export(hosted=None, ultrafast=None, mind2web=None)
    assert any("runs without a final capture of 200: 200" in note for note in candidate.groups[0].limits)


def test_legacy_bu_capture_protocol_is_preserved(tmp_path: Path) -> None:
    h = Harness(tmp_path, bu=refit(screenshot_timing=official.LEGACY_BU_FRAME_TIMINGS[0]))
    candidate = h.export(hosted=None, ultrafast=None, mind2web=None)
    assert any(official.LEGACY_BU_FRAME_TIMINGS[0] in note for note in candidate.groups[0].limits)


def test_bu_capture_availability_must_match_saved_artifact(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    (h.bu / f"{h.ids['bu-bench'][0]}-0" / "final.png").write_bytes(b"frame")
    with pytest.raises(PublicationError, match="capture availability"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_official_publication_states_the_uncapped_protocol(tmp_path: Path) -> None:
    candidate = Harness(tmp_path).export(hosted=None, ultrafast=None, mind2web=None)
    limits = candidate.groups[0].limits
    assert (
        "No time, step or spend limit on any agent; the campaign budget only gates new runs; concurrency: 1" in limits
    )
    assert not any("Model call limits" in note or "Step cap" in note for note in limits)


@pytest.mark.parametrize(
    "edit",
    [
        refit(agent_limits=Limits(max_llm_calls=120, max_jev_calls=500).model_dump(mode="json")),
        refit(agent_limits={"max_llm_calls": None}),
        refit(max_steps=100),
        refit(task_timeout=3600.0),
        refit(agent_budget=1.0),
        refit(budget_policy="remaining-campaign-v1"),
        refit(budget_policy="reserve-next-task-v1"),
    ],
)
def test_official_publication_rejects_any_recorded_cap(tmp_path: Path, edit: Any) -> None:
    h = Harness(tmp_path, bu=edit)
    with pytest.raises(PublicationError, match="uncapped|agent limit"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


@pytest.mark.parametrize("where", ["attempts.jsonl", "results.jsonl"])
@pytest.mark.parametrize("change", [{"max_steps": 100}, {"max_dollars": 2.5}, {"max_seconds": 3600.0}, None])
def test_official_publication_rejects_a_capped_or_unrecorded_physical_attempt(
    tmp_path: Path, where: str, change: dict[str, Any] | None
) -> None:
    h = Harness(tmp_path)
    path = h.bu / where
    rows = publication.lines(path)
    if change is None:
        rows[0].pop("agent_limits")
    else:
        rows[0]["agent_limits"] = UNCAPPED | change
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError, match="agent limit"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_official_physical_spend_cannot_exceed_the_budget(tmp_path: Path) -> None:
    h = Harness(tmp_path, bu=lambda m: m.update(total_budget=10.0))
    with pytest.raises(PublicationError, match="exceeds its approved campaign budget"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_overlapping_official_holds_that_never_fit_cannot_publish(tmp_path: Path) -> None:
    """Total spend (200 x 0.12 = 24) fits a 30 dollar budget, but 21 concurrent 1.5 dollar holds (31.5) never did."""
    h = Harness(tmp_path, bu=lambda m: m.update(total_budget=30.0))
    ids = [f"{task}-0" for task in h.ids["bu-bench"]]
    write_official_journal(h.bu, [ids[:21], *[[task] for task in ids[21:]]], 30.0)
    with pytest.raises(PublicationError, match="budget gate should have refused"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_overlapping_official_holds_within_the_budget_publish(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    ids = [f"{task}-0" for task in h.ids["bu-bench"]]
    write_official_journal(h.bu, [ids[i : i + 3] for i in range(0, len(ids), 3)], 100.0)
    assert h.export(hosted=None, ultrafast=None, mind2web=None).groups[0].id == "bu-bench"


@pytest.mark.parametrize("damage", ["missing", "settlement", "hold"])
def test_an_official_journal_that_does_not_replay_cannot_publish(tmp_path: Path, damage: str) -> None:
    h = Harness(tmp_path)
    journal = h.bu / official.JOURNAL
    if damage == "missing":
        journal.unlink()
    else:
        receipts = [json.loads(line) for line in journal.read_text().splitlines()]
        receipts[1 if damage == "settlement" else 0]["committed" if damage == "settlement" else "reservation"] = 0.9
        journal.write_text("".join(json.dumps(r) + "\n" for r in receipts))
    with pytest.raises(PublicationError, match="journal"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_hosted_publication_states_the_cost_override_and_provider_session_maximum(tmp_path: Path) -> None:
    candidate = Harness(tmp_path).export(ultrafast=None, bu_bench=None, mind2web=None)
    limits = candidate.groups[0].limits
    assert "No time, step or spend limit on any agent; the campaign budget only gates new runs" in limits
    assert "Hosted Browser Use max_cost_usd set to 1000 to override its provider default" in limits
    assert any("4 hour maximum session" in note for note in limits)
    assert (
        "Browser per arm: browser-harness browser-use-cloud; browser-use browser-use-hosted; "
        "browser-use-ultrafast browser-use-hosted; fastbrowse browser-use-cloud" in limits
    )
    assert any(note.startswith("Concurrency 8; budget 1000 dollars") for note in limits)
    assert len(limits) <= 12


def internal_fastbrowse_ledger(tmp_path: Path, judge_cost: float = 0, **over: Any) -> Path:
    def priced(attempts: list[dict[str, Any]]) -> None:
        for a in attempts:
            a["semantic_judge"] = semantic_receipt(a["passed"], judge_cost)

    over = {"browsers": {"fastbrowse": LOCAL_CHROME}, "browser_reserve_dollars": 0.0, "concurrency": 1} | over
    return write_ledger(tmp_path, "internal-fastbrowse", ("fastbrowse",), priced if judge_cost else None, **over)


def test_complete_uncapped_internal_fastbrowse_exports_without_comparators(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    group = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert len(group.group.task_ids) == 53 and len(group.group.rows) == 159
    assert {row.arm for row in group.group.rows} == {"fastbrowse"}
    assert all(row.physical_runs == 1 for row in group.group.rows)
    assert "Browser per arm: fastbrowse local-chrome" in group.group.limits
    # A local browser touches no Browser Use session, so no provider session maximum applies.
    assert not any("4 hour" in note for note in group.group.limits)


def test_internal_fastbrowse_may_run_on_the_cloud_browser(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path, browsers={"fastbrowse": BROWSERS["fastbrowse"]})
    group = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert "Browser per arm: fastbrowse browser-use-cloud" in group.group.limits
    assert any("4 hour maximum session" in note for note in group.group.limits)


@pytest.mark.parametrize(
    "failure",
    ["missing", "caps", "missing_cap", "browser", "browser_hash", "headed", "policy", "unknown", "budget"],
)
def test_internal_fastbrowse_rejects_incomplete_or_changed_physical_protocol(tmp_path: Path, failure: str) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    path = results.with_suffix(".attempts.jsonl")
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    if failure == "missing":
        rows.pop()
    elif failure == "caps":
        rows[0]["run"]["agent_limits"]["max_steps"] = 4
    elif failure == "missing_cap":
        rows[0]["run"]["agent_limits"].pop("max_seconds")
    elif failure == "browser":
        rows[0]["run"]["browsers"]["fastbrowse"]["mode"] = "cloud"
    elif failure == "browser_hash":
        rows[0]["run"]["browsers"]["fastbrowse"].pop("sha256")
    elif failure == "headed":
        rows[0]["run"]["browsers"]["fastbrowse"]["headed"] = True
    elif failure == "policy":
        rows[0]["run"]["budget_policy"] = "remaining-campaign-v1"
    elif failure == "unknown":
        rows[0]["unknown_cost"] = True
    else:
        for row in rows:
            row["dollars"] = 1.0
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


@pytest.mark.parametrize("key", ["max_steps", "max_seconds", "max_llm_calls", "max_jev_calls", "max_dollars"])
def test_internal_publication_rejects_every_recorded_cap_on_every_row(tmp_path: Path, key: str) -> None:
    capped = UNCAPPED | {key: 7}
    results = internal_fastbrowse_ledger(tmp_path, agent_limits=capped)
    with pytest.raises(PublicationError, match="limit was recorded"):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


def test_internal_only_candidate_checks_readback_fixture_and_canonical_catalog(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    candidate = h.export(
        hosted=None,
        ultrafast=None,
        bu_bench=None,
        mind2web=None,
        internal_fastbrowse=internal_fastbrowse_ledger(tmp_path),
    )
    assert candidate.schema_version == publication.timed_schema(3)
    assert [group.id for group in candidate.groups] == ["internal-fastbrowse"]
    assert len(h.read) == 1 and h.fixtures == [(77, AGENT)]
    from fastbrowse.evals.benchmarks import check_catalog

    check_catalog(candidate, h.catalog)


@pytest.mark.parametrize(
    "changed", [{"mode": "local-chrome"}, {"headed": True}, {"profile": True}, {"sha256": "bad"}, {"version": ""}]
)
def test_internal_browser_identity_is_required_even_when_every_row_agrees(
    tmp_path: Path, changed: dict[str, Any]
) -> None:
    browser = changed if set(changed) == {"mode"} else LOCAL_CHROME | changed
    results = internal_fastbrowse_ledger(tmp_path, browsers={"fastbrowse": browser})
    with pytest.raises(PublicationError, match="complete headless"):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


def test_internal_journal_is_read_by_key_when_attempt_writes_are_reordered(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    path = results.with_suffix(".attempts.jsonl")
    rows = path.read_text().splitlines()
    path.write_text("\n".join(reversed(rows)) + "\n")
    assert len(publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST).group.rows) == 159


def test_internal_journal_bytes_change_the_private_experiment_identity(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    [before] = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST).tracked
    journal = results.with_suffix(".budget.jsonl")
    journal.write_bytes(journal.read_bytes() + b"\n")
    [after] = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST).tracked
    assert before.run_id != after.run_id
    assert before.runs[0].name != after.runs[0].name


def test_internal_publication_records_text_models(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    collected = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert "Recorded text models for fastbrowse: google:planner-model, google:reader-model" in collected.group.limits


@pytest.mark.parametrize("value", [None, "", " "])
def test_internal_publication_requires_text_models(tmp_path: Path, value: str | None) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    for path in (results, results.with_suffix(".attempts.jsonl")):
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        for row in rows:
            row["text_model"] = value
        path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError, match="text model"):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


def test_internal_campaign_settlement_includes_paid_semantic_judgments(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path, judge_cost=0.001)
    collected = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert len(collected.group.rows) == 159
    scores = {score.name: score.value for score in collected.tracked[0].runs[0].scores}
    assert scores["known_judge_cost_usd"] == pytest.approx(0.159)


def test_internal_settlement_without_its_judge_spend_cannot_publish(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path, judge_cost=0.001)
    journal_path = results.with_suffix(".budget.jsonl")
    rows = publication.lines(journal_path)
    rows[1]["committed"] -= 0.001  # the agent's charge alone, as if the judge had cost nothing
    journal_path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError, match="settled journal charge"):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


def test_start_gate_journal_accepts_an_overrun_that_stopped_nothing(tmp_path: Path) -> None:
    """One run cost ten times its hold: recorded as it is, and the runs after it still started under the gate."""

    def overrun(attempts: list[dict[str, Any]]) -> None:
        attempts[0]["dollars"] = 10.0

    results = write_ledger(
        tmp_path,
        "internal-fastbrowse",
        ("fastbrowse",),
        overrun,
        browsers={"fastbrowse": LOCAL_CHROME},
        browser_reserve_dollars=0.0,
        concurrency=1,
    )
    group = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert max(row.dollars for row in group.group.rows) == 10.0


@pytest.mark.parametrize("mutation", ["over_gate", "hold", "settlement", "duplicate_settle", "policy", "missing"])
def test_start_gate_journal_rejects_a_start_the_gate_should_have_refused(tmp_path: Path, mutation: str) -> None:
    results = internal_fastbrowse_ledger(tmp_path)
    journal_path = results.with_suffix(".budget.jsonl")
    rows = publication.lines(journal_path)
    if mutation == "over_gate":
        # Hold every reservation before any settles: the 51st two-dollar hold would exceed the 100 dollar budget.
        hold = rows[0]["committed"]
        rows = [r for r in rows if r["committed"] == hold] + [r for r in rows if r["committed"] != hold]
    elif mutation == "hold":
        rows[0]["committed"] = 0.5
    elif mutation == "settlement":
        rows[1]["committed"] = 0.02
    elif mutation == "duplicate_settle":
        rows[3] = rows[1]
    elif mutation == "policy":
        rows[0]["policy"] = "remaining-campaign-v1"
    else:
        rows.pop()
    journal_path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


def backend_ledger(tmp_path: Path, **over: Any) -> Path:
    return write_ledger(tmp_path, "backend", ("fastbrowse", "fastbrowse-pool"), **over)


def test_backend_group_compares_the_two_browser_backends_on_all_53_tasks(tmp_path: Path) -> None:
    collected = publication.internal_group("internal-backend", backend_ledger(tmp_path), CATALOG, DIGEST)
    group = collected.group
    assert len(group.task_ids) == 53 and len(group.rows) == 53 * 3 * 2
    assert {row.arm for row in group.rows} == {"fastbrowse", "fastbrowse-pool"}
    assert all(row.completed for row in group.rows if row.status == "complete")
    assert "Browser per arm: fastbrowse browser-use-cloud; fastbrowse-pool fastbrowse-pool" in group.limits
    assert not any("pool.internal" in note for note in group.limits)


def test_backend_candidate_is_schema_four(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    candidate = h.export(hosted=None, ultrafast=None, bu_bench=None, mind2web=None, backend=backend_ledger(tmp_path))
    assert candidate.schema_version == publication.timed_schema(4) and [g.id for g in candidate.groups] == [
        "internal-backend"
    ]


@pytest.mark.parametrize(
    "over",
    [
        {"browsers": {"fastbrowse": BROWSERS["fastbrowse"]}},
        {"browsers": {"fastbrowse": BROWSERS["fastbrowse"], "fastbrowse-pool": {"mode": "fastbrowse-pool"}}},
        {"browsers": {"fastbrowse": BROWSERS["fastbrowse"], "fastbrowse-pool": BROWSERS["jev-ultrafast"]}},
        {"max_seconds": 300},
    ],
)
def test_backend_group_requires_each_browser_and_no_cap(tmp_path: Path, over: dict[str, Any]) -> None:
    with pytest.raises(PublicationError):
        publication.internal_group("internal-backend", backend_ledger(tmp_path, **over), CATALOG, DIGEST)


def test_backend_group_spec_matches_the_catalog() -> None:
    assert publication.GROUPS["internal-backend"] == (53, 3, "task-success", {"fastbrowse", "fastbrowse-pool"})
    assert publication.INTERNAL["internal-backend"] == "fastbrowse-pool"
    pool = {t["id"] for suite in CATALOG["suites"].values() for t in suite if "fastbrowse-pool" in t["arms"]}
    fast = {t["id"] for suite in CATALOG["suites"].values() for t in suite if "fastbrowse" in t["arms"]}
    assert pool == fast and len(pool) == 53
    assert "max_steps" not in CATALOG


def judged_outage(attempt: dict[str, Any], **report: Any) -> dict[str, Any]:
    """The superseded run a judge found unjudgeable for infrastructure, as the harness now retries it: UNKNOWN to the
    judge, an outage to the harness, kept with its trace, spend and complete judge receipt."""
    outage = json.loads(json.dumps(attempt)) | report
    outage |= {"selected": False, "normalized_status": "unavailable", "passed": None, "seconds": 8.0, "dollars": 0.3}
    decisions = outage["semantic_judge"]["judgment"]["decisions"]
    decisions["evidence_available"] = {"passed": False, "source": "llm", "reason": "Infrastructure failure"}
    decisions["task_satisfied"] = {"passed": False, "source": "jev", "reason": "No answer"}
    return outage


@pytest.mark.parametrize(
    ("group", "arms", "arm", "report"),
    [
        ("internal-hosted", ("fastbrowse", "browser-use"), "browser-use", {"status": "stopped"}),
        (
            "internal-backend",
            ("fastbrowse", "fastbrowse-pool"),
            "fastbrowse-pool",
            {"status": "error", "error": "Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)"},
        ),
    ],
)
def test_a_slot_retried_after_an_infrastructure_unknown_publishes_every_physical_run(
    tmp_path: Path, group: str, arms: tuple[str, ...], arm: str, report: dict[str, Any]
) -> None:
    def retry(attempts: list[dict[str, Any]]) -> None:
        final = next(a for a in attempts if a["arm"] == arm and a["passed"])
        final["retries"] = 1
        attempts.insert(attempts.index(final), judged_outage(final, retries=0, **report))

    collected = publication.internal_group(group, write_ledger(tmp_path, group, arms, retry), CATALOG, DIGEST)
    [retried] = [row for row in collected.group.rows if row.physical_runs == 2]
    assert (retried.arm, retried.score) == (arm, 1.0)
    single = next(row for row in collected.group.rows if row.arm == arm and row.physical_runs == 1)
    assert retried.seconds == pytest.approx(8.0 + single.seconds)
    assert retried.dollars == pytest.approx(0.3 + single.dollars)
    assert collected.physical_runs == len(collected.group.rows) + 1


def _unjudgeable(status: str) -> Callable[[list[dict[str, Any]]], None]:
    def unknown(attempts: list[dict[str, Any]]) -> None:
        index = next(i for i, a in enumerate(attempts) if a["arm"] == "browser-use")
        attempts[index] = judged_outage(attempts[index], status=status) | {"selected": True}

    return unknown


def test_a_slot_our_harness_failed_is_listed_apart_and_the_group_still_publishes(tmp_path: Path) -> None:
    """A run that ended in an error of the harness or its provider, which the judge found unavailable, measured no
    agent: neither a pass nor a failure, out of the rows and recorded with its reason."""
    results = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), _unjudgeable("error"))
    group = publication.internal_group("internal-hosted", results, CATALOG, DIGEST).group
    [slot] = group.unjudged
    assert (slot.arm, slot.reason) == ("browser-use", "evidence-unavailable")
    assert not any((r.arm, r.task, r.repeat) == (slot.arm, slot.task, slot.repeat) for r in group.rows)
    assert group.model_dump(mode="json")["unjudged"] == [slot.model_dump(mode="json")]


def test_a_slot_the_site_blocked_is_the_agents_failure(tmp_path: Path) -> None:
    """Another agent may get past the bot check that stopped this one, so a run that ended `blocked` with no verdict
    is a failed row that names why, not a slot left out."""
    results = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), _unjudgeable("blocked"))
    group = publication.internal_group("internal-hosted", results, CATALOG, DIGEST).group
    assert not group.unjudged
    [row] = [r for r in group.rows if r.failure is not None]
    assert (row.arm, row.score, row.failure) == ("browser-use", 0.0, publication.SITE_BLOCKED)
    assert len([r for r in group.rows if r.arm == "browser-use"]) == 3 * (53 - len(group.excluded))


def test_an_official_run_publishes_every_repeat_and_states_the_per_task_mean(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    shutil.rmtree(h.bu)
    h.bu, _ = write_official(tmp_path, "bu-bench", 200, repeats=3)
    group = h.export(hosted=None, ultrafast=None, mind2web=None).groups[0]
    assert group.repeats == 3
    assert {(r.task, r.repeat) for r in group.rows} == {(t, r) for t in h.ids["bu-bench"] for r in range(3)}
    # Each repeat keeps its own upstream score.
    assert {r.score for r in group.rows if r.task == "bu1"} == {0.5, 0.0}
    assert any("mean of its graded repeat scores" in limit for limit in group.limits)


def test_an_official_run_missing_one_repeat_cannot_publish(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    shutil.rmtree(h.bu)
    h.bu, _ = write_official(tmp_path, "bu-bench", 200, repeats=2)
    results = (h.bu / "results.jsonl").read_text().splitlines()
    (h.bu / "results.jsonl").write_text("\n".join(results[:-1]) + "\n")
    with pytest.raises(PublicationError, match="every task and repeat"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


@pytest.mark.parametrize(
    "policy",
    [
        "browser-internal-jev-v3",
        "browser-internal-opus-v1",
        "browser-internal-opus-v2",
        "browser-internal-opus-v3",
        "browser-internal-opus-v4",
    ],
)
def test_an_internal_ledger_graded_under_another_policy_cannot_publish(tmp_path: Path, policy: str) -> None:
    def other(attempts: list[dict[str, Any]]) -> None:
        for a in attempts:
            a["semantic_judge"]["judgment"]["policy"] = policy

    h = Harness(tmp_path, hosted=other)
    with pytest.raises(PublicationError, match="browser-internal-opus-v5"):
        h.export(ultrafast=None, bu_bench=None, mind2web=None)


HOSTED_TASKS = ("fastbrowse",)


def test_each_measured_arm_records_the_version_and_models_it_ran(tmp_path: Path) -> None:
    """A competitor's pin and the models its rows reported reach the candidate, so a reader of it need not keep its
    own copy that goes stale when a pin changes."""

    def report(attempts: list[dict[str, Any]]) -> None:
        for a in attempts:
            if a["arm"] == "browser-use":
                a["model"] = "anthropic/claude-opus-4.7"
            if a["arm"] == "jev-ultrafast":
                a["model"], a["text_model"] = "jev-model", live.ULTRAFAST_TEXT_MODEL

    arms = ("fastbrowse", "browser-use", "jev-ultrafast")
    ledger = write_ledger(tmp_path, "hosted", arms, report, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    sources = {s.arm: s for s in group.sources}
    assert sources["browser-use"].version == "browser-use-sdk==3.12.1 API v4; default model, reported per row"
    assert sources["browser-use"].models == ["anthropic:claude-opus-4.7"]
    assert sources["jev-ultrafast"].version == f"jev-ultrafast commit {live.ULTRAFAST_COMMIT}"
    assert sources["jev-ultrafast"].models == ["google:gemini-3.1-flash-lite", "jev-model"]
    # fastbrowse's build is the source's agent commit.
    assert sources["fastbrowse"].version is None


def test_a_row_states_what_browser_setup_stayed_on_its_clock(tmp_path: Path) -> None:
    """Hosted Browser Use connects to its browser where the harness cannot see, so its rows say that connection is in
    their seconds, and a consumer need not know which agents do."""

    def note(attempts: list[dict[str, Any]]) -> None:
        for a in attempts:
            if a["arm"] == "browser-use":
                a["timing_note"] = live.HOSTED_SETUP

    arms = ("fastbrowse", "browser-use")
    ledger = write_ledger(tmp_path, "hosted", arms, note, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    assert {r.timing_note for r in group.rows if r.arm == "browser-use"} == {live.HOSTED_SETUP}
    assert {r.timing_note for r in group.rows if r.arm == "fastbrowse"} == {None}
    assert all("timing_note" not in r.model_dump() for r in group.rows if r.arm == "fastbrowse")


def test_a_slot_whose_tries_ran_on_different_clocks_cannot_publish(tmp_path: Path) -> None:
    def retry_on_another_clock(attempts: list[dict[str, Any]]) -> None:
        first = next(a for a in attempts if a["arm"] == "browser-use")
        earlier = dict(first, selected=False, status="unavailable", normalized_status="unavailable", passed=False)
        earlier["semantic_judge"] = json.loads(json.dumps(first["semantic_judge"]))
        earlier["semantic_judge"]["judgment"]["decisions"]["task_satisfied"]["passed"] = False
        earlier["timing_note"], first["retries"] = live.HOSTED_SETUP, 1
        attempts.insert(attempts.index(first), earlier)

    arms = ("fastbrowse", "browser-use")
    ledger = write_ledger(tmp_path, "hosted", arms, retry_on_another_clock, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    with pytest.raises(PublicationError, match="browser setup"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def test_the_hosted_group_measures_whichever_hosted_arms_ran_on_one_task_set(tmp_path: Path) -> None:
    arms = ("fastbrowse", "browser-use", "jev-ultrafast")
    ledger = write_ledger(tmp_path, "hosted", arms, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    assert len(group.task_ids) == 53
    counts = {arm: sum(r.arm == arm for r in group.rows) for arm in arms}
    # Each arm runs every task it can attempt, three times each.
    assert counts == {"fastbrowse": 159, "browser-use": 156, "jev-ultrafast": 18}
    # Each task an arm cannot run is listed with the catalog's reason, and stands for three failed attempts.
    assert [(e.arm, e.task) for e in group.excluded] == sorted(
        (arm, t["id"]) for arm in arms for t in entries(HOSTED_TASKS) if arm not in t["arms"]
    )
    assert len(group.excluded) == 1 + 47
    pause = next(e for e in group.excluded if e.arm == "browser-use")
    assert pause.reason == next(t for t in entries(HOSTED_TASKS) if t["id"] == pause.task)["excluded"]["browser-use"]
    assert any("failed where it cannot run: browser-use 1, jev-ultrafast 47" in n for n in group.limits)
    assert any("jev-ultrafast text helper" in n for n in group.limits)

    # browser-harness, a coding agent given no credentials, runs only the set's credential-free completion tasks.
    harness = write_ledger(tmp_path, "harness", ("fastbrowse", "browser-harness"), task_arms=HOSTED_TASKS)
    group = publication.internal_group("internal-hosted", harness, CATALOG, DIGEST).group
    supported = [t for t in entries(HOSTED_TASKS) if "browser-harness" in t["arms"]]
    assert 0 < len(supported) < 53
    assert sum(r.arm == "browser-harness" for r in group.rows) == 3 * len(supported)
    assert any(f"browser-harness {53 - len(supported)}" in n for n in group.limits)

    alone = write_ledger(tmp_path, "alone", ("fastbrowse",), task_arms=HOSTED_TASKS)
    group = publication.internal_group("internal-hosted", alone, CATALOG, DIGEST).group
    assert {r.arm for r in group.rows} == {"fastbrowse"} and len(group.rows) == 159


def parallax_rows(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """The rows and notes as parallax builds them, before the public contract validates them."""
    built: dict[str, Any] = {}

    def build_group(group_id, source, task_ids, limits, rows, run, repeats=None):
        built.update(rows=rows, limits=limits)
        return SimpleNamespace(rows=rows)

    monkeypatch.setattr(publication, "Row", lambda **row: SimpleNamespace(**row))
    monkeypatch.setattr(publication, "build_group", build_group)
    return built


@pytest.mark.parametrize(
    ("arms", "edit", "message"),
    [
        (("fastbrowse", "fastbrowse-pool"), None, "must be some of"),
    ],
)
def test_the_hosted_group_refuses_foreign_arms_and_invented_or_missing_rows(
    tmp_path: Path, arms: tuple[str, ...], edit: Any, message: str
) -> None:
    ledger = write_ledger(tmp_path, "hosted", arms, edit, task_arms=HOSTED_TASKS)
    with pytest.raises(PublicationError, match=message):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def refused(attempt: dict[str, Any]) -> None:
    """An attempt its provider refused to start: no run, no judge, a known zero charge (`live.run_arm`)."""
    attempt.update(started=False, status=None, normalized_status="error", passed=None, semantic_judge=None, seconds=0.0)
    attempt.update(dollars=0.0, unknown_cost=False, failure="NotStarted: browser agent run returned HTTP 401")


def refuse_arm(arm: str, keep: int) -> Callable[[list[dict[str, Any]]], None]:
    def edit(attempts: list[dict[str, Any]]) -> None:
        for attempt in [a for a in attempts if a["arm"] == arm][keep:]:
            refused(attempt)

    return edit


def test_an_arm_that_never_started_is_not_measured_and_blocks_nothing(tmp_path: Path) -> None:
    """An arm whose key is refused refuses every start: it is left out like an arm that never ran, and the others
    publish."""
    arms = ("fastbrowse", "browser-use", "jev-ultrafast")
    ledger = write_ledger(
        tmp_path, "hosted", arms, refuse_arm("jev-ultrafast", 0), task_arms=HOSTED_TASKS, budget_usd=1000.0
    )
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    assert {r.arm for r in group.rows} == {"fastbrowse", "browser-use"}
    assert any("jev-ultrafast not measured, no run started" in n for n in group.limits)


def test_one_refused_start_among_started_runs_still_blocks(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    parallax_rows(monkeypatch)
    arms = ("fastbrowse", "browser-use")
    ledger = write_ledger(
        tmp_path, "hosted", arms, refuse_arm("browser-use", 1), task_arms=HOSTED_TASKS, budget_usd=1000.0
    )
    with pytest.raises(PublicationError, match="complete semantic judge receipt"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def block_slot(
    h: Harness, retries: int | None = None, *, finding: bool = True, task: str = "bu0", status: str | None = None
) -> None:
    """Rewrite one BU Bench slot as the runner leaves one the site blocked on every retry."""
    retries = official.OUTAGE_RETRIES if retries is None else retries
    decision = {"passed": not finding, "source": "llm", "reason": "the site served an error page"}
    judgment = {
        "policy": official.INTERRUPTION_POLICY,
        "state_sha256": "0" * 64,
        "questions": official.INTERRUPTION_QUESTIONS,
        "decisions": {"evidence_available": decision},
    }
    check = {"judgment": judgment, "policy_sha256": "f" * 64, "judge_cost_usd": 0.01}
    path, folder = h.bu / "results.jsonl", h.bu / f"{task}-0"
    if status is not None:
        saved = json.loads((folder / "agent.json").read_text())
        (folder / "agent.json").write_text(json.dumps(saved | {"status": status}))
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    keys = ("status", "agent_dollars", "agent_cost_unknown", "seconds", "timing_basis")
    agent = {k: v for k, v in rows[0].items() if k in keys}
    agent |= {"status": status, "completed": False} if status is not None else {"completed": True}
    earlier = []
    for n in range(retries):
        kept = h.bu / f"{task}-0.interrupted-{n}"
        kept.mkdir()
        for name in ("agent.json", "timing.json"):
            (kept / name).write_bytes((folder / name).read_bytes())
        earlier.append(
            agent | {"folder": kept.name, "judge_usd": 0.0, "interruption_check": check, "interruption_check_usd": 0.01}
        )
    (folder / "receipts.jsonl").unlink()
    index = next(i for i, r in enumerate(rows) if r["task"] == task)
    rows[index] = {k: v for k, v in rows[index].items() if k != "final_capture_available"} | agent | {
        "upstream_score": None, "upstream_passed": None, "error_type": "Interrupted", "judge_usd": 0.0,
        "interruptions": earlier, "interruption_check": check, "interruption_check_usd": 0.01,
    }  # fmt: skip
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    journal = [json.loads(line) for line in (h.bu / official.JOURNAL).read_text().splitlines()]
    journal = [r for r in journal if r["id"] != f"{task}-0"]
    for n in range(retries + 1):
        name, charge = official.run_id(task, 0, n), 0.11
        journal += [official.journal_row(name, 100.0, 1.5, 1.5), official.journal_row(name, 100.0, 1.5, charge)]
    (h.bu / official.JOURNAL).write_text("".join(json.dumps(r) + "\n" for r in journal))


def test_a_slot_the_site_refused_on_every_retry_is_a_failed_row(tmp_path: Path) -> None:
    """Another agent may get past what stopped this one, so an official slot the site refused on every retry is the
    agent's failed attempt, as an internal one is, never left out of the score."""
    h = Harness(tmp_path)
    block_slot(h)
    candidate = h.export(hosted=None, ultrafast=None, mind2web=None)
    [group] = candidate.groups
    assert group.blocked == []
    [row] = [r for r in group.rows if r.task == "bu0"]
    assert (row.repeat, row.arm, row.score) == (0, "fastbrowse", 0.0)
    assert row.failure == publication.SITE_REFUSED
    assert row.physical_runs == official.OUTAGE_RETRIES + 1
    assert row.seconds == 3.0 * (official.OUTAGE_RETRIES + 1)
    assert len(group.rows) == 200
    assert any("refused on every retry is a failed attempt" in limit for limit in group.limits)


def test_an_official_slot_that_ended_blocked_names_the_bot_check(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    block_slot(h, status="blocked")
    [group] = h.export(hosted=None, ultrafast=None, mind2web=None).groups
    [row] = [r for r in group.rows if r.task == "bu0"]
    assert (row.status, row.completed, row.score, row.failure) == ("blocked", False, 0.0, publication.SITE_BLOCKED)


@pytest.mark.parametrize(
    ("retries", "finding"),
    [(official.OUTAGE_RETRIES - 1, True), (official.OUTAGE_RETRIES, False)],
    ids=["retries-not-spent", "check-did-not-find-the-site-at-fault"],
)
def test_a_slot_is_blocked_only_on_its_evidence(tmp_path: Path, retries: int, finding: bool) -> None:
    h = Harness(tmp_path)
    block_slot(h, retries, finding=finding)
    with pytest.raises(PublicationError, match="blocked by the site"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_a_sampled_run_cannot_reach_official_rows_or_publication(tmp_path: Path) -> None:
    h = Harness(tmp_path, bu=refit(sample=2))
    with pytest.raises(PublicationError, match="sampled"):
        h.export(hosted=None, ultrafast=None, mind2web=None)
    argv = ["bu-bench", str(h.bu), "--dataset", "d", "--upstream-root", "u", "--judge-python", "p", "--out", "o"]
    with pytest.raises(SystemExit, match="sampled"):
        official_rows.main(argv)


def test_an_ungraded_slot_the_site_did_not_block_still_refuses(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    path = h.bu / "results.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    rows[0] |= {"upstream_score": None, "upstream_passed": None, "error_type": "ValueError"}
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    with pytest.raises(PublicationError, match="source grade differs"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_an_official_slot_that_ended_in_a_harness_error_is_unjudged(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    block_slot(h, status="error")
    [group] = h.export(hosted=None, ultrafast=None, mind2web=None).groups
    assert "bu0" not in {r.task for r in group.rows}
    assert [(u.task, u.repeat, u.reason) for u in group.unjudged] == [("bu0", 0, "evidence-unavailable")]


def test_an_official_slot_counts_the_runs_its_interruption_check_sent_back(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    path = h.bu / "results.jsonl"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    kept = h.bu / "bu0-0.interrupted-0"
    kept.mkdir()
    for name in ("agent.json", "timing.json"):
        (kept / name).write_text((h.bu / "bu0-0" / name).read_text())
    run = {"folder": kept.name, "status": "complete", "agent_dollars": 0.1, "agent_cost_unknown": False}
    run |= {"seconds": 3.0, "timing_basis": TIMING_BASIS, "judge_usd": 0.0, "interruption_check_usd": 0.01}
    run |= {"reason": "error page"}
    rows[0] |= {"interruptions": [run], "interruption_check_usd": 0.01}
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    ids = [f"{task}-0" for task in h.ids["bu-bench"]]
    charges = {i: 0.12 for i in ids} | {"bu0-0": 0.11, official.run_id("bu0", 0, 1): 0.13}
    journal = [official.journal_row(i, 100.0, 1.5, c) for i, charge in charges.items() for c in (1.5, charge)]
    (h.bu / official.JOURNAL).write_text("".join(json.dumps(r) + "\n" for r in journal))
    group = h.export(hosted=None, ultrafast=None, mind2web=None).groups[0]
    [slot] = [r for r in group.rows if r.physical_runs == 2]
    assert slot.physical_runs == 2 and slot.seconds == 6.0 and slot.dollars == pytest.approx(0.2)

    (kept / "receipts.jsonl").write_text("")
    with pytest.raises(PublicationError, match="sent to the upstream judge"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def refreshed(tmp_path: Path) -> tuple[Any, Any]:
    arms = ("fastbrowse", "browser-use", "browser-use-ultrafast", "browser-harness")
    ledger = write_ledger(tmp_path, "refresh", arms, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    live = write_ledger(tmp_path, "live", ("fastbrowse", "jev-ultrafast"), task_arms=HOSTED_TASKS, budget_usd=1000.0)
    return SimpleNamespace(group=group), publication.internal_group("internal-hosted", live, CATALOG, DIGEST)


def reference_file(tmp_path: Path, **over: Any) -> Path:
    fresh, _ = refreshed(tmp_path)
    candidate = SimpleNamespace(groups=[fresh.group], catalog_sha256=DIGEST, campaign_id="refresh-1")
    made = publication.competitor_reference(candidate, CATALOG, "2026-11-01").model_dump(mode="json") | over
    path = tmp_path / "reference.json"
    path.write_text(json.dumps(made))
    return path


def test_no_reference_refuses_the_comparison(tmp_path: Path) -> None:
    _, live = refreshed(tmp_path)
    with pytest.raises(PublicationError, match="refresh_competitors"):
        publication.with_reference(live, tmp_path / "missing.json", CATALOG)


def test_a_reference_under_another_policy_or_a_changed_task_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(publication, "reference_basis", lambda _: TIMING_BASIS)
    other = tmp_path / "a"
    other.mkdir()
    with pytest.raises(PublicationError, match="graded under"):
        path = reference_file(other, judge_policy="browser-internal-opus-v2")
        publication.with_reference(refreshed(other)[1], path, CATALOG)
    stale = tmp_path / "b"
    stale.mkdir()
    fresh = reference_file(stale)
    body = json.loads(fresh.read_text())
    body["task_versions"]["pypi-version"] += 1
    fresh.write_text(json.dumps(body))
    with pytest.raises(PublicationError, match="browser-use/pypi-version"):
        publication.with_reference(refreshed(stale)[1], fresh, CATALOG)


def test_a_default_campaign_publishes_on_the_reference_and_a_refresh_writes_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(publication, "reference_basis", lambda _: TIMING_BASIS)
    h = Harness(tmp_path)
    refresh = h.export(ultrafast=None, bu_bench=None, mind2web=None)
    assert not refresh.groups[0].references
    made = publication.competitor_reference(refresh, json.loads(h.catalog.read_bytes()), "2026-11-01")
    path = tmp_path / "competitor-reference.json"
    path.write_text(made.model_dump_json())
    live = write_ledger(tmp_path, "live", ("fastbrowse", "jev-ultrafast"), task_arms=HOSTED_TASKS, budget_usd=1000.0)
    group = h.export(hosted=live, ultrafast=None, bu_bench=None, mind2web=None, reference=path).groups[0]
    assert {r.arm for r in group.rows} == {
        "fastbrowse",
        "jev-ultrafast",
        "browser-use",
        "browser-use-ultrafast",
        "browser-harness",
    }
    assert [a.vendor for a in group.references] == ["Browser Use", "Browser Use Ultrafast", "Browser Harness"]
    assert [a.models for a in group.references] == [["model-x"]] * 3
    with pytest.raises(PublicationError, match="refresh_competitors"):
        h.export(hosted=live, ultrafast=None, bu_bench=None, mind2web=None)


def test_export_retains_a_physical_reference_clock(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    refresh = h.export(ultrafast=None, bu_bench=None, mind2web=None)
    reference = publication.competitor_reference(refresh, CATALOG, "2026-11-01")
    body = reference.model_dump(mode="json")
    physical = next(
        row for row in body["rows"] if (row["arm"], row["task"], row["repeat"]) == ("browser-use", "pypi-version", 1)
    )
    for row in body["rows"]:
        if row["arm"] == "browser-use":
            row["dollars"] = None
    physical.update(
        status="completed",
        completed=True,
        score=1.0,
        llm_calls=None,
        jev_calls=0,
        tokens_per_second=0.0,
        input_tokens=34339,
        output_tokens=225,
        seconds=340.3,
        dollars=None,
        physical_runs=2,
        timing_basis="physical-attempt-wall-time",
        timing_note=publication.PHYSICAL_TIMING_NOTE,
    )
    physical.pop("session_open_seconds", None)
    physical.pop("session_close_seconds", None)
    path = tmp_path / "reference.json"
    path.write_text(json.dumps(body))
    live = write_ledger(tmp_path, "live", ("fastbrowse", "jev-ultrafast"), task_arms=HOSTED_TASKS, budget_usd=1000.0)
    candidate = h.export(hosted=live, ultrafast=None, bu_bench=None, mind2web=None, reference=path)
    group = Candidate.model_validate_json(candidate.model_dump_json()).groups[0]
    retained = [row.model_dump(mode="json") for row in group.rows if row.arm in publication.REFERENCE_ARMS]
    assert retained == sorted(body["rows"], key=lambda row: (row["arm"], row["task"], row["repeat"]))
    assert group.references == reference.arms
    assert publication.MIXED_TIMING_NOTE in group.limits
    assert publication.TIMING_NOTE not in group.limits


def test_a_reference_with_an_unrecorded_clock_is_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = reference_file(tmp_path)
    monkeypatch.setattr(publication, "reference_basis", lambda _: None)
    with pytest.raises(PublicationError, match="unrecorded clock"):
        publication.with_reference(refreshed(tmp_path)[1], path, CATALOG)


@pytest.mark.parametrize("basis", [None, "session-included", "wall"])
def test_seconds_on_another_clock_never_publish(tmp_path: Path, basis: str | None) -> None:
    h = Harness(tmp_path, hosted=lambda a: a[0].update(timing_basis=basis))
    with pytest.raises(PublicationError, match="session ready to the answer"):
        h.export(ultrafast=None, bu_bench=None, mind2web=None)


@pytest.mark.parametrize("field", ["timing_basis", "session_open_limit_seconds"])
def test_a_run_or_manifest_must_record_the_clock_and_its_open_limit(tmp_path: Path, field: str) -> None:
    h = Harness(tmp_path, hosted_run={field: None})
    with pytest.raises(PublicationError, match="session"):
        h.export(ultrafast=None, bu_bench=None, mind2web=None)
    (tmp_path / "official").mkdir()
    h = Harness(tmp_path / "official", bu=refit(**{field: None}))
    with pytest.raises(PublicationError, match="session"):
        h.export(hosted=None, ultrafast=None, mind2web=None)


def test_public_rows_carry_the_basis_and_session_seconds(monkeypatch: pytest.MonkeyPatch) -> None:
    class Contract:
        model_fields = {"timing_basis": None}

    monkeypatch.setattr(publication, "Row", Contract)
    tries = [{"session_open_seconds": 2.0, "session_close_seconds": None}, {"session_open_seconds": 1.5}]
    assert publication.timing_fields(tries) == {
        "timing_basis": TIMING_BASIS,
        "session_open_seconds": 3.5,
        "session_close_seconds": None,
    }


@pytest.mark.parametrize(
    ("attempts", "expected"),
    [
        ([{"llm_calls": 2}, {"llm_calls": 3}], {"llm_calls": 5}),
        (
            [
                {"llm_calls": 2, "llm_calls_basis": "physical_requests"},
                {"llm_calls": 3, "llm_calls_basis": "physical_requests"},
            ],
            {"llm_calls": 5, "llm_calls_basis": "physical_requests"},
        ),
        ([{"llm_calls": 2}, {}], {"llm_calls": None}),
        ([{"llm_calls": 2}, {"llm_calls": 3, "llm_calls_basis": "physical_requests"}], {"llm_calls": None}),
        ([{"llm_calls_basis": "physical_requests"}], {"llm_calls": None, "llm_calls_basis": "physical_requests"}),
        (
            [{"llm_calls": 0, "llm_calls_basis": "physical_requests"}],
            {"llm_calls": 0, "llm_calls_basis": "physical_requests"},
        ),
    ],
)
def test_retry_call_counts_preserve_basis_and_unknown(attempts: list[dict[str, Any]], expected: dict[str, Any]) -> None:
    assert publication.call_counts(attempts) == expected


@pytest.mark.parametrize("value", [-1, True, 1.5, "2"])
def test_retry_call_counts_refuse_invalid_counts(value: Any) -> None:
    with pytest.raises(PublicationError, match="llm_calls"):
        publication.call_counts([{"llm_calls": value}])


@pytest.mark.parametrize("basis", ["", "unknown", False, [], {}])
def test_retry_call_counts_refuse_invalid_basis(basis: Any) -> None:
    with pytest.raises(PublicationError, match="llm_calls_basis"):
        publication.call_counts([{"llm_calls": 1, "llm_calls_basis": basis}])


def test_candidate_export_keeps_physical_call_basis_and_unknown_count(tmp_path: Path) -> None:
    def physical(attempts: list[dict[str, Any]]) -> None:
        for attempt in attempts:
            attempt.update(llm_calls=2, llm_calls_basis="physical_requests")
        attempts[0]["llm_calls"] = None

    candidate = Harness(tmp_path, hosted=physical).export()
    rows = Candidate.model_validate_json(candidate.model_dump_json()).groups[0].rows
    assert any(row.llm_calls is None and row.llm_calls_basis == "physical_requests" for row in rows)
    assert any(row.llm_calls == 2 and row.llm_calls_basis == "physical_requests" for row in rows)


def narrowed(tasks: set[str], fail: bool = False) -> Callable[[list[dict[str, Any]]], None]:
    """A rerun of only `tasks`, as the campaign's `only` input runs it; `fail` makes every one of its slots fail."""

    def edit(attempts: list[dict[str, Any]]) -> None:
        attempts[:] = [a for a in attempts if a["task"] in tasks]
        for a in attempts if fail else ():
            a["passed"], a["semantic_judge"] = False, semantic_receipt(False)

    return edit


LATER: dict[str, Any] = {"run_id": "r2", "git_sha": "c" * 40, "run_started": "2026-10-08T09:00:00+00:00"}


def test_each_task_comes_from_the_newest_run_that_measured_it_in_full(tmp_path: Path) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    full = internal_fastbrowse_ledger(first)
    rerun = {entries(("fastbrowse",))[0]["id"]}
    late = write_ledger(
        second, "internal-fastbrowse", ("fastbrowse",), narrowed(rerun, fail=True),
        browsers={"fastbrowse": LOCAL_CHROME}, browser_reserve_dollars=0.0, concurrency=1, **LATER,
    )  # fmt: skip
    # The order given does not matter: the recorded start decides which run is newest.
    collected = publication.internal_group("internal-fastbrowse", [full, late], CATALOG, DIGEST)
    group = collected.group
    assert {r.score for r in group.rows if r.task in rerun} == {0.0}
    assert len(group.rows) == 159 and len(collected.tracked) == 2
    assert [(s.run_id, s.agent_sha, s.tasks) for s in group.sources][0] == ("r2", "c" * 40, sorted(rerun))
    assert group.sources[1].agent_sha == AGENT and len(group.sources[1].tasks) == 52
    assert group.agent_sha == "c" * 40


def test_a_rerun_supplies_only_the_tasks_it_measured_in_full(tmp_path: Path) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    full = internal_fastbrowse_ledger(first)
    task = entries(("fastbrowse",))[0]["id"]

    def partial(attempts: list[dict[str, Any]]) -> None:
        narrowed({task}, fail=True)(attempts)
        attempts.pop()  # one repeat never ran

    late = write_ledger(
        second, "internal-fastbrowse", ("fastbrowse",), partial,
        browsers={"fastbrowse": LOCAL_CHROME}, browser_reserve_dollars=0.0, concurrency=1, **LATER,
    )  # fmt: skip
    # The rerun measured nothing in full, so the first run supplies every task and the rerun is not read.
    group = publication.internal_group("internal-fastbrowse", [late, full], CATALOG, DIGEST).group
    assert [s.run_id for s in group.sources] == ["r1"]
    # A rerun alone does not cover the catalog.
    with pytest.raises(PublicationError, match="no run measured"):
        publication.internal_group("internal-fastbrowse", late, CATALOG, DIGEST)


def test_a_slot_measured_on_an_earlier_task_version_is_listed_apart(tmp_path: Path) -> None:
    task = entries(("fastbrowse",))[0]

    def earlier(attempts: list[dict[str, Any]]) -> None:
        for a in attempts:
            if a["task"] == task["id"]:
                a["task_version"] = task["task_version"] - 1

    results = write_ledger(
        tmp_path, "internal-fastbrowse", ("fastbrowse",), earlier,
        browsers={"fastbrowse": LOCAL_CHROME}, browser_reserve_dollars=0.0, concurrency=1,
    )  # fmt: skip
    group = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST).group
    assert {(s.task, s.reason) for s in group.unjudged} == {(task["id"], "task-changed")}
    assert len(group.unjudged) == 3 and len(group.rows) == 156


def test_an_arm_remeasured_on_a_new_pin_leaves_the_earlier_runs_other_arms_publishable(tmp_path: Path) -> None:
    """A competitor re-run on a new version supplies that arm; the earlier run keeps supplying the arms it alone
    measured, and only the pins of the arms a run supplies are held to the catalog."""
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    old = {"pin": "browser-use-sdk==3.0.0 API v3", "tier": "hosted"}
    arms = ("fastbrowse", "browser-use", "browser-harness")
    earlier = write_ledger(first, "hosted", arms, task_arms=HOSTED_TASKS, budget_usd=1000.0)
    for line in (earlier, earlier.with_suffix(".attempts.jsonl")):
        rows = [json.loads(r) for r in line.read_text().splitlines()]
        for row in rows:
            row["run"]["arms"]["browser-use"] = old
        line.write_text("".join(json.dumps(r) + "\n" for r in rows))
    competitors = ("fastbrowse", "browser-use", "browser-use-ultrafast", "jev-ultrafast")
    newer = write_ledger(second, "hosted", competitors, task_arms=HOSTED_TASKS, budget_usd=1000.0, **LATER)
    group = publication.internal_group("internal-hosted", [earlier, newer], CATALOG, DIGEST).group
    assert {(s.run_id, s.arm) for s in group.sources} == {
        ("r2", "fastbrowse"), ("r2", "browser-use"), ("r2", "browser-use-ultrafast"), ("r2", "jev-ultrafast"),
        ("r1", "browser-harness"),
    }  # fmt: skip
    # Alone, the earlier run would publish browser-use on a pin the catalog no longer names.
    with pytest.raises(PublicationError, match="catalog pins"):
        publication.internal_group("internal-hosted", earlier, CATALOG, DIGEST)


def test_the_head_to_head_takes_fastbrowse_from_a_newer_fastbrowse_only_run(tmp_path: Path) -> None:
    """As the release gate pools them, fastbrowse's head-to-head tasks come from the newest run that measured
    fastbrowse there, a fastbrowse-only run included; each competitor keeps the run it was measured in."""
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    duel = write_ledger(first, "hosted", ("fastbrowse", "browser-use"))
    newer = dict(browsers={"fastbrowse": BROWSERS["fastbrowse"]}, browser_reserve_dollars=0.0, concurrency=1, **LATER)
    every = {t["id"] for t in entries(("fastbrowse",))}
    alone = write_ledger(second, "internal-fastbrowse", ("fastbrowse",), narrowed(every, fail=True), **newer)
    group = publication.internal_group("internal-hosted", [duel, alone], CATALOG, DIGEST).group
    assert {r.score for r in group.rows if r.arm == "fastbrowse"} == {0.0}
    assert [(s.run_id, s.arm, len(s.tasks)) for s in group.sources] == [
        ("r2", "fastbrowse", 53),
        ("r1", "browser-use", 52),
    ]
    assert group.agent_sha == "c" * 40
    # fastbrowse on another browser in the newer run is not the same measurement.
    other = dict(newer, browsers={"fastbrowse": LOCAL_CHROME})
    moved = write_ledger(second, "internal-fastbrowse", ("fastbrowse",), **other)
    with pytest.raises(PublicationError, match="drove fastbrowse on different browsers"):
        publication.internal_group("internal-hosted", [duel, moved], CATALOG, DIGEST)


@pytest.mark.parametrize("arm", ["browser-use", "browser-harness"])
@pytest.mark.parametrize("partial", [None, 0.3])
def test_unresolved_comparator_cost_keeps_outcome_time_and_private_receipts(
    tmp_path: Path, arm: str, partial: float | None
) -> None:
    def unknown(attempts: list[dict[str, Any]]) -> None:
        next(a for a in attempts if a["arm"] == arm).update(dollars=partial, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", arm), unknown, task_arms=HOSTED_TASKS)
    sources = [ledger, ledger.with_suffix(".attempts.jsonl"), ledger.with_suffix(".budget.jsonl")]
    original = [p.read_bytes() for p in sources]
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    theirs = [r for r in group.rows if r.arm == arm]
    assert all(r.dollars is None and r.seconds == 2.0 for r in theirs)
    assert {r.score for r in theirs} == {0.0, 1.0}
    assert all(r.dollars == 0.25 for r in group.rows if r.arm == "fastbrowse")
    assert [p.read_bytes() for p in sources] == original


def test_an_unknown_retry_withholds_the_selected_slots_cost(tmp_path: Path) -> None:
    def retry(attempts: list[dict[str, Any]]) -> None:
        selected = next(a for a in attempts if a["arm"] == "browser-use")
        earlier = dict(selected, selected=False, dollars=None, unknown_cost=True)
        selected["retries"] = 1
        attempts.insert(attempts.index(selected), earlier)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), retry)
    group = publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST).group
    retried = next(r for r in group.rows if r.physical_runs == 2)
    assert retried.dollars is None and retried.seconds == 4.0
    assert all(r.dollars is None for r in group.rows if r.arm == "browser-use")


def test_a_newer_unknown_cost_withholds_the_arms_older_priced_slots(tmp_path: Path) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    arms = ("fastbrowse", "browser-use")
    earlier = write_ledger(first, "hosted", arms)
    task = next(t["id"] for t in entries(arms))

    def unknown(attempts: list[dict[str, Any]]) -> None:
        narrowed({task})(attempts)
        next(a for a in attempts if a["arm"] == "browser-use").update(dollars=None, unknown_cost=True)

    newer = write_ledger(second, "hosted", arms, unknown, **LATER)
    group = publication.internal_group("internal-hosted", [earlier, newer], CATALOG, DIGEST).group
    assert len(group.rows) == (53 + 52) * 3
    assert all(r.dollars is None for r in group.rows if r.arm == "browser-use")
    assert all(r.dollars == 0.25 for r in group.rows if r.arm == "fastbrowse")
    assert {s.run_id for s in group.sources if s.arm == "browser-use"} == {"r1", "r2"}


def test_an_unknown_comparator_charge_cannot_release_its_hold(tmp_path: Path) -> None:
    def unknown(attempts: list[dict[str, Any]]) -> None:
        next(a for a in attempts if a["arm"] == "browser-use").update(dollars=None, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), unknown)
    path = ledger.with_suffix(".budget.jsonl")
    journal = [json.loads(line) for line in path.read_text().splitlines()]
    indices = [n for n, row in enumerate(journal) if row["id"].startswith("browser-use/")]
    journal[indices[1]]["committed"] = 0.0
    path.write_text("".join(json.dumps(r) + "\n" for r in journal))
    with pytest.raises(PublicationError, match="settled journal charge"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


@pytest.mark.parametrize("value", [-0.1, float("nan"), float("inf"), True])
def test_even_an_unknown_partial_receipt_requires_finite_nonnegative_dollars(tmp_path: Path, value: Any) -> None:
    def invalid(attempts: list[dict[str, Any]]) -> None:
        next(a for a in attempts if a["arm"] == "browser-use").update(dollars=value, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), invalid)
    with pytest.raises(PublicationError, match="dollars"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def test_an_unknown_agent_charge_cannot_hide_an_unpriced_judge(tmp_path: Path) -> None:
    def unknown(attempts: list[dict[str, Any]]) -> None:
        next(a for a in attempts if a["arm"] == "browser-use").update(dollars=None, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), unknown)
    for path in (ledger, ledger.with_suffix(".attempts.jsonl")):
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        row = next(r for r in rows if r["unknown_cost"])
        row["semantic_judge"]["judgment"]["fallback_calls"] = [
            {"model": OPUS_JUDGE, "attempt": 1, "latency_ms": 1, "cost_usd": None}
        ]
        path.write_text("".join(json.dumps(r) + "\n" for r in rows))
    with pytest.raises(PublicationError, match="unpriced judge"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def test_unresolved_holds_still_enforce_the_campaign_start_budget(tmp_path: Path) -> None:
    def unknown(attempts: list[dict[str, Any]]) -> None:
        for row in attempts:
            if row["arm"] == "browser-use":
                row.update(dollars=None, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), unknown)
    with pytest.raises(PublicationError, match="budget gate should have refused"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


def test_selected_results_cannot_change_a_partial_receipts_unknown_flag(tmp_path: Path) -> None:
    def partial(attempts: list[dict[str, Any]]) -> None:
        next(a for a in attempts if a["arm"] == "browser-use").update(dollars=0.3, unknown_cost=True)

    ledger = write_ledger(tmp_path, "hosted", ("fastbrowse", "browser-use"), partial)
    rows = [json.loads(line) for line in ledger.read_text().splitlines()]
    next(row for row in rows if row["unknown_cost"])["unknown_cost"] = False
    ledger.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError, match="differs from its selected attempt"):
        publication.internal_group("internal-hosted", ledger, CATALOG, DIGEST)


@pytest.mark.parametrize("concurrency", [4, 8])
def test_internal_publication_preserves_explicit_concurrency(tmp_path: Path, concurrency: int) -> None:
    results = internal_fastbrowse_ledger(tmp_path, concurrency=concurrency)
    group = publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)
    assert any(note.startswith(f"Concurrency {concurrency};") for note in group.group.limits)


def test_internal_publication_rejects_relabeling_attempt_concurrency(tmp_path: Path) -> None:
    results = internal_fastbrowse_ledger(tmp_path, concurrency=4)
    attempts = results.with_suffix(".attempts.jsonl")
    rows = publication.lines(attempts)
    rows[0]["concurrency"] = 8
    attempts.write_text("".join(json.dumps(row) + "\n" for row in rows))
    with pytest.raises(PublicationError, match="concurrency differs from the protocol"):
        publication.internal_group("internal-fastbrowse", results, CATALOG, DIGEST)


@pytest.mark.parametrize("new_concurrency,reported", [(4, "4"), (8, "4 or 8")])
def test_internal_publication_reports_each_retained_invocation_concurrency(
    tmp_path: Path, new_concurrency: int, reported: str
) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    first.mkdir(), second.mkdir()
    duel = write_ledger(first, "hosted", ("fastbrowse", "browser-use"), concurrency=4)
    alone = write_ledger(second, "internal-fastbrowse", ("fastbrowse",), concurrency=new_concurrency, **LATER)
    group = publication.internal_group("internal-hosted", [duel, alone], CATALOG, DIGEST).group
    assert any(note.startswith(f"Concurrency {reported};") for note in group.limits)
