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
    }


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
