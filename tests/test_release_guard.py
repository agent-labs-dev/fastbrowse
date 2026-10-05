"""The release guard: every release needs a green Evals run, and a comparison the same code it measured."""

import importlib.util
import io
import json
import subprocess
from pathlib import Path

import pytest

_spec = importlib.util.spec_from_file_location(
    "release_guard", Path(__file__).parents[1] / "scripts" / "release_guard.py"
)
assert _spec is not None and _spec.loader is not None
release_guard = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release_guard)


@pytest.fixture(autouse=True)
def _stub_publication_gate(monkeypatch: pytest.MonkeyPatch) -> None:
    """The ancestry and green checks are what these tests cover; the gate reuse has its own test below."""
    monkeypatch.setattr(release_guard, "publication_problems", lambda *_: [])


def _root(tmp_path: Path, version: str, rows: list[dict]) -> Path:
    (tmp_path / "docs" / "results").mkdir(parents=True, exist_ok=True)
    (tmp_path / "docs" / "results" / f"{version}.jsonl").write_text(
        "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
    )
    return tmp_path


def _row(*, sha: str = "a" * 40, dirty: bool = False) -> dict:
    return {"run": {"fastbrowse_version": "0.5.19", "git_sha": sha, "git_dirty": dirty}}


def _git(path: Path, *args: str) -> str:
    done = subprocess.run(["git", *args], cwd=path, check=True, capture_output=True, text=True)
    return done.stdout.strip()


def _init(path: Path) -> None:
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=path, check=True, capture_output=True)
    _git(path, "config", "user.email", "test@example.com")
    _git(path, "config", "user.name", "Test")


def _commit(path: Path, message: str) -> None:
    _git(path, "add", "-A")
    subprocess.run(["git", "commit", "-q", "-m", message], cwd=path, check=True, capture_output=True)


def test_a_release_without_a_comparison_still_needs_a_green_eval_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: False)
    issues = release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=tmp_path)
    assert any("no successful" in issue for issue in issues), issues


def test_a_release_without_a_comparison_passes_with_a_green_eval_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    assert release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=tmp_path) == []


def test_a_release_without_a_comparison_still_queries_the_release_build(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[tuple] = []
    monkeypatch.setattr(release_guard, "_evals_green", lambda *args: calls.append(args) or True)
    release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=tmp_path)
    assert calls == [("org/repo", "b" * 40, "token")]


@pytest.mark.parametrize("head", ["HEAD", "main", "0123456789abcdef", "-n1", ""])
def test_a_malformed_release_build_is_refused_without_a_query(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, head: str
) -> None:
    """A ref name, an option or an abbreviation must fail closed rather than query the wrong build."""
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: pytest.fail("queried a malformed release build"))
    issues = release_guard.problems("0.5.19", head, "org/repo", "token", root=tmp_path)
    assert any("full 40-hex" in issue for issue in issues), issues


def test_cli_requires_a_release_commit_even_without_a_comparison(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.delenv("GITHUB_SHA", raising=False)
    assert release_guard.main(["9.9.9", "--token", "token"]) == 1
    assert "no release commit" in capsys.readouterr().out


def test_cli_requires_a_token_even_without_a_comparison(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.delenv("GH_TOKEN", raising=False)
    assert release_guard.main(["9.9.9", "--head", "b" * 40]) == 1
    assert "no GitHub token" in capsys.readouterr().out


def test_a_query_error_fails_closed_even_without_a_comparison(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def _boom(*_args: object) -> bool:
        raise RuntimeError("cannot read evals.yml runs from GitHub: connection reset")

    monkeypatch.setattr(release_guard, "_evals_green", _boom)
    assert release_guard.main(["9.9.9", "--head", "b" * 40, "--token", "token"]) == 1
    assert "cannot read" in capsys.readouterr().out


def test_evals_green_fails_closed_on_a_malformed_query_response(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(release_guard.urllib.request, "urlopen", lambda *_args, **_kwargs: io.StringIO("[]"))
    with pytest.raises(RuntimeError):
        release_guard._evals_green("org/repo", "b" * 40, "token")


def test_a_comparison_needs_a_green_eval_run_on_the_release_build(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = _root(tmp_path, "0.5.19", [_row()])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: False)
    monkeypatch.setattr(release_guard, "in_release", lambda *_: True)
    monkeypatch.setattr(release_guard, "code_differences", lambda *_: [])
    issues = release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=root)
    assert any("no successful" in issue for issue in issues)


def test_a_measured_build_must_be_in_the_release_history(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root = _root(tmp_path, "0.5.19", [_row()])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    monkeypatch.setattr(release_guard, "in_release", lambda *_: False)
    issues = release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=root)
    assert any("not in the release's history" in issue for issue in issues)


@pytest.mark.parametrize("sha", ["HEAD", "main", "0123456789abcdef", "-n1"])
def test_a_measured_build_must_be_a_full_commit_sha(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sha: str) -> None:
    """A ref name, an option or an abbreviation is refused before git is asked to resolve it."""
    repo, head = _measured_repo(tmp_path)
    _root(repo, "0.5.19", [_row(sha=sha)])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    issues = release_guard.problems("0.5.19", head, "org/repo", "token", root=repo)
    assert any("full 40-hex" in issue for issue in issues), issues


def test_in_release_rejects_a_ref_name_and_an_option(tmp_path: Path) -> None:
    repo, head = _measured_repo(tmp_path)
    assert release_guard.in_release(repo, "HEAD", head) is False
    assert release_guard.in_release(repo, "-n1", head) is False
    assert release_guard.in_release(repo, head, head) is True


def test_a_dirty_comparison_build_is_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root = _root(tmp_path, "0.5.19", [_row(dirty=True)])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    monkeypatch.setattr(release_guard, "in_release", lambda *_: True)
    monkeypatch.setattr(release_guard, "code_differences", lambda *_: [])
    issues = release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=root)
    assert any("unclean" in issue for issue in issues)


def test_a_clean_comparison_inside_history_passes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    root = _root(tmp_path, "0.5.19", [_row()])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    monkeypatch.setattr(release_guard, "in_release", lambda *_: True)
    monkeypatch.setattr(release_guard, "code_differences", lambda *_: [])
    assert release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=root) == []


def test_release_guard_reuses_the_publication_gate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A comparison the publication gate would block cannot ship even with a green Evals run."""
    root = _root(tmp_path, "0.5.19", [_row()])
    monkeypatch.setattr(release_guard, "publication_problems", release_guard._publication_problems)
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    monkeypatch.setattr(release_guard, "in_release", lambda *_: True)
    monkeypatch.setattr(release_guard, "code_differences", lambda *_: [])
    issues = release_guard.problems("0.5.19", "b" * 40, "org/repo", "token", root=root)
    assert any("publication ledger" in issue for issue in issues), issues


def _measured_repo(tmp_path: Path) -> tuple[Path, str]:
    """A real repository whose measured build has one source file, with its sha."""
    repo = tmp_path / "repo"
    (repo / "src").mkdir(parents=True)
    _init(repo)
    (repo / "src" / "agent.py").write_text("VALUE = 1\n", encoding="utf-8")
    _commit(repo, "measured build")
    return repo, _git(repo, "rev-parse", "HEAD")


def test_a_later_code_change_breaks_code_identity(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    repo, measured = _measured_repo(tmp_path)
    (repo / "src" / "agent.py").write_text("VALUE = 2\n", encoding="utf-8")
    _commit(repo, "later runtime change")
    head = _git(repo, "rev-parse", "HEAD")
    _root(repo, "0.5.19", [_row(sha=measured)])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    issues = release_guard.problems("0.5.19", head, "org/repo", "token", root=repo)
    assert any("different executable code" in issue for issue in issues), issues


def test_a_later_docs_and_steering_commit_keeps_code_identity(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    repo, measured = _measured_repo(tmp_path)
    (repo / "docs").mkdir()
    (repo / "docs" / "note.md").write_text("evidence\n", encoding="utf-8")
    (repo / "README.md").write_text("readme\n", encoding="utf-8")
    (repo / ".agents").mkdir()
    (repo / ".agents" / "steering.md").write_text("steer\n", encoding="utf-8")
    _commit(repo, "evidence follows the measurement")
    head = _git(repo, "rev-parse", "HEAD")
    _root(repo, "0.5.19", [_row(sha=measured)])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    assert release_guard.problems("0.5.19", head, "org/repo", "token", root=repo) == []


def test_a_source_file_moved_into_docs_is_still_a_code_change(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Rename detection reports a move by its new path only; without it the deleted source path still refuses."""
    repo, measured = _measured_repo(tmp_path)
    (repo / "docs").mkdir()
    _git(repo, "mv", "src/agent.py", "docs/agent.py")
    _commit(repo, "move executable code into docs")
    head = _git(repo, "rev-parse", "HEAD")
    # The old source path is the difference that matters, so it must not be hidden behind the new evidence path.
    assert "src/agent.py" in release_guard.code_differences(repo, measured, head)
    _root(repo, "0.5.19", [_row(sha=measured)])
    monkeypatch.setattr(release_guard, "_evals_green", lambda *_: True)
    issues = release_guard.problems("0.5.19", head, "org/repo", "token", root=repo)
    assert any("different executable code" in issue for issue in issues), issues


@pytest.mark.parametrize(
    "change,green",
    [
        ({}, True),
        ({"head_sha": "a" * 40}, False),
        ({"head_branch": "feature"}, False),
        ({"status": "in_progress"}, False),
        ({"conclusion": "failure"}, False),
        ({"conclusion": "cancelled"}, False),
        ({"conclusion": "skipped"}, False),
    ],
)
def test_evals_green_checks_the_returned_run_not_only_the_count(
    monkeypatch: pytest.MonkeyPatch, change: dict, green: bool
) -> None:
    run = {"head_sha": "b" * 40, "head_branch": "main", "status": "completed", "conclusion": "success"} | change
    body = json.dumps({"total_count": 1, "workflow_runs": [run]})
    monkeypatch.setattr(release_guard.urllib.request, "urlopen", lambda *_args, **_kwargs: io.StringIO(body))
    assert release_guard._evals_green("org/repo", "b" * 40, "token") is green


def test_evals_green_refuses_a_positive_count_without_a_run(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        release_guard.urllib.request, "urlopen", lambda *_args, **_kwargs: io.StringIO('{"total_count": 1}')
    )
    with pytest.raises(RuntimeError, match="unexpected response shape"):
        release_guard._evals_green("org/repo", "b" * 40, "token")
