import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fastbrowse.evals import versions


def test_an_installed_package_cannot_inherit_the_consumers_git_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(versions, "ROOT", tmp_path)
    assert versions._git("rev-parse", "HEAD") is None


def test_a_vcs_install_records_the_agent_commit_from_its_install_receipt(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(versions, "_git", lambda *_: None)
    commit = "a" * 40
    monkeypatch.setattr(
        versions,
        "distribution",
        lambda _: SimpleNamespace(read_text=lambda _: json.dumps({"vcs_info": {"commit_id": commit}})),
    )
    run = versions.provenance()
    assert run["git_sha"] == commit and run["git_dirty"] is False
