import asyncio
import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

SPEC = importlib.util.spec_from_file_location(
    "eval_diagnostics", Path(__file__).parents[1] / "scripts/eval_diagnostics.py"
)
assert SPEC and SPEC.loader
helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(helper)


@pytest.mark.parametrize("value", ["", "0", "-1", "nan", "inf", "-inf"])
def test_positive_budget(value):
    with pytest.raises(ValueError):
        helper.positive(value)


def env(**overrides):
    return {
        "GITHUB_EVENT_NAME": "workflow_dispatch",
        "EVAL_PROBE": '{"task":"read"}',
        "EVAL_PROBE_BUDGET": "1",
        "EVAL_DIAGNOSTIC_CERTIFICATE": "public certificate",
        **overrides,
    }


def test_validation_requires_separate_budget_and_manual_dispatch(monkeypatch):
    calls = []
    monkeypatch.setattr(helper.subprocess, "run", lambda *a, **k: calls.append((a, k)))
    probe, tasks = helper.configuration(env())
    assert probe.task == "read" and tasks == []
    assert calls[0][0][0][:3] == ["openssl", "cms", "-encrypt"]
    for changed in (
        {"EVAL_PROBE_BUDGET": ""},
        {"GITHUB_EVENT_NAME": "schedule"},
        {"EVAL_FILL_BUDGET": "2"},
        {"EVAL_PROBE": '{"task":"read","fetch":"true"}'},
        {"EVAL_PROBE": '{"task":"read","authorization":true}'},
        {"EVAL_PROBE": '{"task":"read","output_schema":{"type":"array"}}'},
        {"EVAL_FILL_TASKS": '["x","x"]'},
        {"EVAL_FILL_TASKS": '"x"'},
    ):
        with pytest.raises(ValueError):
            helper.configuration(env(**changed))


def test_schedule_has_no_extra_spend():
    assert helper.configuration({"GITHUB_EVENT_NAME": "schedule"}) == (None, [])


def test_invalid_recipient_fails_before_execution(monkeypatch):
    def invalid(*args, **kwargs):
        raise subprocess.CalledProcessError(1, args[0])

    monkeypatch.setattr(helper.subprocess, "run", invalid)
    with pytest.raises(subprocess.CalledProcessError):
        helper.configuration(env())


def test_probe_preserves_schema_trace_cost_and_budget_on_failure(tmp_path, monkeypatch):
    import fastbrowse
    from fastbrowse import traced
    from fastbrowse.telemetry import collecting

    calls = []
    schema = {"type": "object", "properties": {"package": {"type": "string"}}, "required": ["package"]}

    async def run(task, **kwargs):
        calls.append((task, kwargs))
        events = collecting()
        assert events is not None
        events.append({"event": "http", "request": "full original payload"})
        raise RuntimeError("provider failure")

    monkeypatch.setattr(fastbrowse, "run_task", run)
    monkeypatch.setattr(helper, "identity", lambda: {"git_sha": "exact", "native_sha256": "native"})
    with pytest.raises(RuntimeError):
        asyncio.run(helper.run_probe(helper.Probe(task="read", fetch=True, output_schema=schema), 2.5, tmp_path))
    assert calls[0][1]["limits"].max_dollars == 2.5
    assert calls[0][1]["output_schema"].model_json_schema() == schema
    assert json.loads((tmp_path / "probe.receipt.json").read_text())["unresolved_budget_hold_usd"] == 2.5
    assert json.loads((tmp_path / "probe.trace.jsonl").read_text())["request"] == "full original payload"
    with traced() as events:
        assert events == []


def test_workflow_diagnostics_cannot_replace_full_fixture_gate():
    workflow = yaml.safe_load((Path(__file__).parents[1] / ".github/workflows/evals.yml").read_text())
    job = workflow["jobs"]["fixtures"]
    assert job["runs-on"].startswith("blacksmith-")
    steps = job["steps"]
    fixture = next(s for s in steps if s.get("id") == "fixtures")
    assert "if" not in fixture
    assert "--suite local mock" in fixture["run"]
    assert '--repeat "$EVAL_REPEAT"' in fixture["run"]
    for name in ("Check the stability budget", "Check the publication gate"):
        step = next(s for s in steps if s.get("name") == name)
        assert step["if"] == "always() && steps.fixtures.outcome != 'skipped'"
        assert "nightly.jsonl" in step["run"]
    upload = steps[-1]["with"]["path"].splitlines()
    assert upload == ["artifacts/evals/nightly*", "artifacts/evals/diagnostics.cms"]
    for step in steps:
        assert "${{ inputs." not in step.get("run", "")


def test_fill_uses_argv_and_rejects_incomplete_or_unpriced_rows(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(helper, "configuration", lambda _: (None, ["fixture-id"]))
    monkeypatch.setattr(helper, "identity", lambda: {"git_sha": "exact"})
    monkeypatch.setenv("EVAL_FILL_BUDGET", "3")
    monkeypatch.setattr(sys, "argv", ["eval_diagnostics", "fill"])
    seen = []

    def run(command, **kwargs):
        seen.append(command)
        Path("artifacts/evals/diagnostics/fill.json").write_text(json.dumps({"tasks": []}))
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(helper.subprocess, "run", run)
    assert helper.main() == 1
    assert seen[0][-2:] == ["--only", "fixture-id"]
    assert seen[0][seen[0].index("--cassettes") + 1] == str(tmp_path / "bench/cassettes")
    assert "shell" not in seen[0]
    receipt = json.loads(Path("artifacts/evals/diagnostics/fill.receipt.json").read_text())
    assert receipt["unresolved_budget_hold_usd"] == 3
    assert receipt["complete"] is False


def test_encryption_retains_raw_artifacts_without_plaintext_upload(tmp_path, monkeypatch):
    import io
    import tarfile

    monkeypatch.chdir(tmp_path)
    key, cert = tmp_path / "key.pem", tmp_path / "cert.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=diagnostic-test",
            "-keyout",
            str(key),
            "-out",
            str(cert),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
    )
    certificate = cert.read_text()
    helper.configuration(env(EVAL_DIAGNOSTIC_CERTIFICATE=certificate))
    monkeypatch.setenv("EVAL_DIAGNOSTIC_CERTIFICATE", certificate)
    raw = Path("artifacts/evals/diagnostics")
    raw.mkdir(parents=True, mode=0o700)
    (raw / "probe.trace.jsonl").write_text("private request payload")
    Path("bench/cassettes").mkdir(parents=True)
    Path("bench/cassettes/task.jsonl.gz").write_bytes(b"cassette payload")
    assert helper.encrypt() == 0
    encrypted = Path("artifacts/evals/diagnostics.cms")
    assert b"private request payload" not in encrypted.read_bytes()
    done = subprocess.run(
        [
            "openssl",
            "cms",
            "-decrypt",
            "-binary",
            "-inform",
            "DER",
            "-in",
            str(encrypted),
            "-recip",
            str(cert),
            "-inkey",
            str(key),
        ],
        capture_output=True,
        check=True,
    )
    with tarfile.open(fileobj=io.BytesIO(done.stdout), mode="r:gz") as archive:
        trace = archive.extractfile("diagnostics/probe.trace.jsonl")
        assert trace is not None and trace.read() == b"private request payload"
        assert "diagnostics/raw.tar.gz" not in archive.getnames()
        cassette = archive.extractfile("cassettes/task.jsonl.gz")
        assert cassette is not None and cassette.read() == b"cassette payload"


@pytest.mark.parametrize(
    "changes,expected",
    [
        ({}, 0),
        ({"unpriced_exchanges": 1}, 1),
        ({"recorded_cost_usd": 4.0}, 1),
        ({"misses": ["missing request"]}, 1),
        ({"result": {"grade": {"passed": False}}}, 1),
    ],
)
def test_fill_requires_priced_complete_converged_result(tmp_path, monkeypatch, changes, expected):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(helper, "configuration", lambda _: (None, ["fixture-id"]))
    monkeypatch.setattr(helper, "identity", lambda: {"git_sha": "exact"})
    monkeypatch.setenv("EVAL_FILL_BUDGET", "3")
    monkeypatch.setattr(sys, "argv", ["eval_diagnostics", "fill"])
    entry = {
        "task": "fixture-id",
        "recorded_cost_usd": 0.5,
        "unpriced_exchanges": 0,
        "misses": [],
        "result": {"grade": {"passed": True}},
        **changes,
    }

    def run(command, **kwargs):
        Path("artifacts/evals/diagnostics/fill.json").write_text(json.dumps({"tasks": [entry]}))
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(helper.subprocess, "run", run)
    assert helper.main() == expected
    receipt = json.loads(Path("artifacts/evals/diagnostics/fill.receipt.json").read_text())
    assert receipt["unresolved_budget_hold_usd"] == 3


def test_identity_refuses_wrong_or_dirty_checkout(tmp_path, monkeypatch):
    from fastbrowse import build

    native = tmp_path / "native.so"
    native.write_bytes(b"native bytes")
    monkeypatch.setattr(helper.importlib, "import_module", lambda _: SimpleNamespace(__file__=str(native)))
    monkeypatch.setenv("GITHUB_SHA", "exact")
    monkeypatch.setattr(build, "identity", lambda: {"git_sha": "wrong", "git_dirty": False})
    with pytest.raises(ValueError):
        helper.identity()
    monkeypatch.setattr(build, "identity", lambda: {"git_sha": "exact", "git_dirty": True})
    with pytest.raises(ValueError):
        helper.identity()
    monkeypatch.setattr(build, "identity", lambda: {"git_sha": "exact", "git_dirty": False})
    assert len(helper.identity()["native_sha256"]) == 64


def test_native_preflight_refuses_all_network_and_browser_work(monkeypatch):

    import fastbrowse

    calls = []

    async def run(task, **kwargs):
        calls.append(kwargs)
        assert kwargs["chrome"] is None
        assert kwargs["settings"]["OPENROUTER_API_KEY"] == "validation-only"
        await kwargs["http"].get("https://example.invalid/")

    monkeypatch.setattr(fastbrowse, "run_task", run)
    asyncio.run(helper.validate_probe(helper.Probe(task="read", fetch=True)))
    assert len(calls) == 1

    async def invalid(task, **kwargs):
        raise ValueError("unsupported schema")

    monkeypatch.setattr(fastbrowse, "run_task", invalid)
    with pytest.raises(ValueError, match="unsupported schema"):
        asyncio.run(helper.validate_probe(helper.Probe(task="read", fetch=True)))


@pytest.mark.parametrize("unknown,known,expected", [(False, 0.5, 0), (True, 0.5, 1), (False, 4.0, 1)])
def test_probe_requires_complete_priced_result_within_budget(tmp_path, monkeypatch, unknown, known, expected):
    import fastbrowse
    from fastbrowse.models import Status

    async def run(task, **kwargs):
        return SimpleNamespace(
            status=Status.COMPLETE,
            model_dump=lambda **_: {"status": "complete"},
            cost=SimpleNamespace(has_unknown=unknown, known_dollars=known, model_dump=lambda **_: {"known": known}),
        )

    monkeypatch.setattr(fastbrowse, "run_task", run)
    monkeypatch.setattr(helper, "identity", lambda: {"git_sha": "exact"})
    assert asyncio.run(helper.run_probe(helper.Probe(task="read", fetch=True), 3.0, tmp_path)) == expected


def test_settings_match_campaign_backend_and_reject_credential_inputs(monkeypatch):
    import runpy

    from fastbrowse.evals import harness

    supplied = {
        "OPENROUTER_API_KEY": "test-provider-key",
        "AI_GATEWAY_API_KEY": "excluded-gateway-key",
        "EXA_API_KEY": "test-search-key",
        "FASTBROWSE_LLM_MODEL": "model-from-original-request",
        "FASTBROWSE_LLM_REASONING": "medium",
        "FASTBROWSE_JEV_SOURCE": "openrouter",
        "BROWSER_USE_API_KEY": "excluded-browser-key",
    }
    monkeypatch.setattr(harness, "environment", lambda: supplied)
    evals_src = Path(__file__).parents[1] / "evals/src"
    # The campaign package is its own project, not installed in the root environment; its backend imports it.
    monkeypatch.syspath_prepend(str(evals_src))
    backend = runpy.run_path(str(evals_src / "fastbrowse_evals/backend.py"))
    probe = helper.Probe(task="read", allowed_origins=["https://example.invalid"])
    assert helper.run_settings(probe) == backend["run_settings"]()
    assert "AI_GATEWAY_API_KEY" not in helper.run_settings(probe)
    for name in ("OPENROUTER_API_KEY", "FASTBROWSE_LLM_BASE_URL"):
        with pytest.raises(ValueError):
            helper.Probe(task="read", settings={name: "not-allowed-in-dispatch"})
    configured = probe.model_copy(update={"settings": {"FASTBROWSE_LLM_REASONING": "high"}})
    assert helper.run_settings(configured)["FASTBROWSE_LLM_REASONING"] == "high"
