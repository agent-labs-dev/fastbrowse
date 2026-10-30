import hashlib
import json
from pathlib import Path

import pytest

from fastbrowse.evals.site_groups import RunFacts, group_files

STAGED = Path("docs/results/benchmark-candidate.json")
PUBLISHED = Path("docs/results/benchmarks.json")
FACTS = RunFacts(run_on="2026-10-30", harness="Parallax", judge="anthropic/claude-opus-5.5")
PUBLISHED_ONLY = {"measured": None, "published": [{"system": "browser-use"}]}
CURRENT = {"live-suite": {"measured": {}}, "online-mind2web": PUBLISHED_ONLY, "bu-bench": PUBLISHED_ONLY}


def test_the_live_suite_names_the_head_to_head_group_and_its_exact_rows() -> None:
    content = STAGED.read_bytes()
    files = group_files(content, FACTS, CURRENT)
    candidate = json.loads(content)
    rows = f"data/benchmark-{candidate['campaign_id']}.json"
    assert set(files) == {rows, "data/benchmarks/live-suite.json"}
    assert files[rows] == content
    live = json.loads(files["data/benchmarks/live-suite.json"])
    assert (live["tasks"], live["repeats"], live["published"]) == (None, None, [])
    assert live["measured"] == {
        "group": "internal-hosted",
        "campaign_id": candidate["campaign_id"],
        "agent_sha": candidate["agent_sha"],
        "candidate_sha256": hashlib.sha256(content).hexdigest(),
        "rows": rows,
        "run_on": "2026-10-30",
        "harness": "Parallax",
        "judge": "anthropic/claude-opus-5.5",
        "systems": {"fastbrowse": "fastbrowse", "browser-use": "browser-use"},
        "cost_not_reported": [],
        "not_measured": ["browser-use-ultrafast", "browserbase"],
    }


def test_a_vendor_without_a_cost_or_without_rows_has_its_own_state() -> None:
    candidate = json.loads(STAGED.read_bytes())
    group = next(g for g in candidate["groups"] if g["id"] == "internal-hosted")
    group["rows"] = [
        {**row, "arm": "browserbase", "dollars": None} if row["arm"] == "browser-use" else row for row in group["rows"]
    ]
    candidate["groups"] = [group]
    content = json.dumps(candidate).encode()
    live = json.loads(group_files(content, FACTS, CURRENT)["data/benchmarks/live-suite.json"])["measured"]
    assert live["systems"] == {"fastbrowse": "fastbrowse", "browserbase": "browserbase"}
    assert live["cost_not_reported"] == ["browserbase"]
    assert live["not_measured"] == ["browser-use", "browser-use-ultrafast"]


def test_a_fastbrowse_only_campaign_shows_fastbrowse_alone() -> None:
    live = json.loads(group_files(PUBLISHED.read_bytes(), FACTS, CURRENT)["data/benchmarks/live-suite.json"])
    assert live["measured"]["group"] == "internal-fastbrowse"
    assert live["measured"]["systems"] == {"fastbrowse": "fastbrowse"}


def test_a_group_bound_to_an_earlier_campaign_is_refused() -> None:
    current = {**CURRENT, "bu-bench": {"measured": {"campaign_id": "earlier"}, "published": []}}
    with pytest.raises(ValueError, match="bu-bench is bound to an earlier campaign"):
        group_files(STAGED.read_bytes(), FACTS, current)


def test_a_campaign_id_that_cannot_name_a_site_file_is_refused() -> None:
    candidate = json.loads(STAGED.read_bytes())
    candidate["campaign_id"] = "release:0.8.0"
    with pytest.raises(ValueError, match="cannot name a site data file"):
        group_files(json.dumps(candidate).encode(), FACTS, CURRENT)


def test_a_measured_official_group_keeps_only_vendor_rows_on_its_task_set() -> None:
    candidate = json.loads(STAGED.read_bytes())
    hosted = next(g for g in candidate["groups"] if g["id"] == "internal-hosted")
    tasks = [f"m2w-{i}" for i in range(300)]
    row = {"status": "complete", "completed": True, "seconds": 2.0, "dollars": 0.1, "physical_runs": 1}
    candidate["groups"] = [
        {
            **{k: hosted[k] for k in ("agent_sha", "runner_sha", "source_sha256", "limits")},
            "id": "online-mind2web",
            "metric": "full-task-success",
            "task_ids": tasks,
            "repeats": 1,
            "rows": [{**row, "task": t, "repeat": 0, "arm": "fastbrowse", "score": i % 2} for i, t in enumerate(tasks)],
        }
    ]
    same = {"system": "browser-use", "rows": [{"task": t, "score": 1} for t in reversed(tasks)]}
    other = {"system": "browser-use", "rows": [{"task": f"x-{t}", "score": 1} for t in tasks]}
    summary = {"system": "browser-use", "rows": None, "summary": {"attempts": 100, "passed": 78}}

    def published(*entries: dict) -> list:
        current = {
            **CURRENT,
            "live-suite": PUBLISHED_ONLY,
            "online-mind2web": {"measured": None, "published": list(entries)},
        }
        files = group_files(json.dumps(candidate).encode(), FACTS, current)
        return json.loads(files["data/benchmarks/online-mind2web.json"])["published"]

    assert published(same) == [same]
    assert published(other, summary) == []
