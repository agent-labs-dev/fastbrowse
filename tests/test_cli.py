import json

import pytest

from fastbrowse import cli
from fastbrowse.models import CostBreakdown, RunResult, Status


def test_a_preflight_error_under_json_still_leaves_a_result_on_stdout(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.delenv("NOT_SET", raising=False)
    monkeypatch.setattr(
        "sys.argv", ["fastbrowse", "t", "--start", "https://example.com", "--json", "--secret", "p=NOT_SET"]
    )
    with pytest.raises(SystemExit) as exit_:
        cli.main()
    captured = capsys.readouterr()
    result = json.loads(captured.out)
    assert result["status"] == "error" and result["answer"] is None and result["steps"] == []
    assert "NOT_SET" in result["error"]
    assert "NOT_SET" in str(exit_.value.code)


def test_a_preflight_error_without_json_writes_nothing_to_stdout(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.delenv("NOT_SET", raising=False)
    monkeypatch.setattr("sys.argv", ["fastbrowse", "t", "--start", "https://example.com", "--secret", "p=NOT_SET"])
    with pytest.raises(SystemExit):
        cli.main()
    assert capsys.readouterr().out == ""


@pytest.mark.parametrize(
    ("flag", "value", "named"), [("--max-steps", "0", "--max-steps"), ("--max-dollars", "-1", "--max-dollars")]
)
def test_a_limit_argparse_accepts_but_the_run_cannot_use_is_refused_as_a_configuration_error(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], flag: str, value: str, named: str
) -> None:
    # `argparse` types these but does not bound them, and the model that does raises a validation error, which
    # would have reached the terminal as a traceback with nothing on stdout for a caller parsing `--json`.
    monkeypatch.setattr("sys.argv", ["fastbrowse", "t", "--start", "https://example.com", "--json", flag, value])
    with pytest.raises(SystemExit) as exit_:
        cli.main()
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "error" and result["steps"] == []
    assert named in result["error"] and named in str(exit_.value.code)


@pytest.mark.parametrize("flag", [["--secret", "p=SET"], ["--bitwarden", "vault-item"]])
def test_a_credential_without_a_start_page_is_refused_rather_than_dropped(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], flag: list[str]
) -> None:
    # A secret is only ever typed on the start origin, so with no page named there is nowhere it could be used.
    monkeypatch.setenv("SET", "value")
    monkeypatch.setattr("sys.argv", ["fastbrowse", "t", "--json", *flag])
    with pytest.raises(SystemExit) as exit_:
        cli.main()
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "error" and "--start" in result["error"]
    assert "--start" in str(exit_.value.code)


def test_cdp_url_hands_run_task_the_url_and_no_cloud_key(monkeypatch: pytest.MonkeyPatch) -> None:
    """The browser is the caller's: `run_task` gets the URL, and nothing starts a second one."""
    seen = {}

    async def fake_run_task(task: str, **kwargs: object) -> RunResult:
        seen.update(kwargs)
        return RunResult(
            status=Status.COMPLETE,
            answer="ok",
            data=None,
            evidence=(),
            steps=(),
            cost=CostBreakdown(lines=()),
            artifacts=(),
            error=None,
        )

    monkeypatch.setattr(cli, "run_task", fake_run_task)
    monkeypatch.setattr(
        "sys.argv", ["fastbrowse", "t", "--start", "https://example.com", "--cdp-url", "ws://browser.test/devtools"]
    )
    with pytest.raises(SystemExit) as exit_:
        cli.main()
    assert exit_.value.code == 0
    assert seen["cdp_url"] == "ws://browser.test/devtools"
    assert seen["browser_api_key"] is None


@pytest.mark.parametrize("flag", [["--local"], ["--headed"], ["--profile", "/tmp/kept"], ["--cloud-profile", "prof_1"]])
def test_cdp_url_refuses_flags_that_shape_a_started_browser(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], flag: list[str]
) -> None:
    monkeypatch.setattr(
        "sys.argv",
        [
            "fastbrowse",
            "t",
            "--start",
            "https://example.com",
            "--json",
            "--cdp-url",
            "ws://browser.test/devtools",
            *flag,
        ],
    )
    with pytest.raises(SystemExit) as exit_:
        cli.main()
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "error" and "--cdp-url" in result["error"] and flag[0] in result["error"]
    assert flag[0] in str(exit_.value.code)


def test_cdp_url_counts_a_profile_from_the_environment(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # FASTBROWSE_PROFILE implies --profile; a handed-over browser refuses it just the same.
    monkeypatch.setenv("FASTBROWSE_PROFILE", "/tmp/kept")
    monkeypatch.setattr(
        "sys.argv",
        ["fastbrowse", "t", "--start", "https://example.com", "--json", "--cdp-url", "ws://browser.test/devtools"],
    )
    with pytest.raises(SystemExit):
        cli.main()
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "error" and "--profile" in result["error"]


def test_cdp_url_requires_a_websocket_url(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    monkeypatch.setattr(
        "sys.argv",
        ["fastbrowse", "t", "--start", "https://example.com", "--json", "--cdp-url", "https://browser.test/devtools"],
    )
    with pytest.raises(SystemExit):
        cli.main()
    result = json.loads(capsys.readouterr().out)
    assert result["status"] == "error" and "ws://" in result["error"]


def test_version_prints_the_installed_version_without_a_task(capsys: pytest.CaptureFixture[str]) -> None:
    """Bug reports ask for it, and a task is a required argument everywhere else."""
    with pytest.raises(SystemExit) as exited:
        cli._parse(["--version"])
    assert exited.value.code == 0
    assert capsys.readouterr().out.startswith("fastbrowse 0.")
