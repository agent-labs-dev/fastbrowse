import json
import re
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

from fastbrowse import build
from fastbrowse.evals import versions
from fastbrowse.models import CliResult


def test_a_checkout_names_its_full_commit_in_every_harness_row() -> None:
    run = versions.provenance()
    assert re.fullmatch(r"[0-9a-f]{40}", str(run["git_sha"])) and isinstance(run["git_dirty"], bool)


def test_an_installed_package_cannot_inherit_the_consumers_git_commit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(build, "ROOT", tmp_path)
    assert build._git("rev-parse", "HEAD") is None


def test_a_vcs_install_records_the_agent_commit_from_its_install_receipt(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(build, "_git", lambda *_: None)
    commit = "a" * 40
    monkeypatch.setattr(
        build,
        "distribution",
        lambda _: SimpleNamespace(read_text=lambda _: json.dumps({"vcs_info": {"commit_id": commit}})),
    )
    run = build.identity()
    assert run["git_sha"] == commit and run["git_dirty"] is False


def test_the_cli_json_result_validates_as_its_declared_model() -> None:
    argv = ["t", "--start", "https://example.com", "--json", "--secret", "p=FB_TEST_UNSET_VARIABLE"]
    script = f"import sys; sys.argv = ['fastbrowse', *{argv!r}]; from fastbrowse.commands import fastbrowse as f; f()"
    ran = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, timeout=60, check=False)
    result = CliResult.model_validate_json(ran.stdout)
    assert result.build.fastbrowse_version and result.error and "FB_TEST_UNSET_VARIABLE" in result.error
