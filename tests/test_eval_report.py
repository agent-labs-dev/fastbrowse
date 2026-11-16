import hashlib
import hmac
import json
from pathlib import Path
from urllib.error import URLError

import pytest

from scripts import eval_report


@pytest.fixture
def configured(monkeypatch: pytest.MonkeyPatch) -> None:
    for key, value in {
        "WEBHOOK_URL": "https://example.invalid/private-token",
        "WEBHOOK_SECRET": "secret-token",
        "GITHUB_REPOSITORY": "agent-labs-dev/fastbrowse",
        "CAMPAIGN_ID": "campaign",
        "GROUP": "Fastbrowse",
        "STATUS": "failure",
        "SMOKE": "true",
        "RUN_URL": "https://example.invalid/run",
    }.items():
        monkeypatch.setenv(key, value)


def test_signature_covers_exact_body_and_preserves_receiver_contract(configured, monkeypatch):
    captured = {}

    class Response:
        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

    def capture(request, timeout):
        captured.update(body=request.data, signature=request.get_header("X-webhook-signature"), timeout=timeout)
        return Response()

    monkeypatch.setattr(eval_report, "urlopen", capture)
    text = "## Results\n\n2 pass, 1 invalid, cost unknown.\nUnicode: café\n"
    eval_report.send(text)
    assert captured["signature"] == hmac.new(b"secret-token", captured["body"], hashlib.sha256).hexdigest()
    assert captured["timeout"] == 10
    assert json.loads(captured["body"]) == {
        "source": "fastbrowse-benchmark-campaign",
        "repository": "agent-labs-dev/fastbrowse",
        "campaign_id": "campaign",
        "group": "Fastbrowse",
        "status": "failure",
        "smoke": True,
        "run_url": "https://example.invalid/run",
        "report_markdown": text,
    }


@pytest.mark.parametrize("missing", [("WEBHOOK_URL",), ("WEBHOOK_SECRET",), ("WEBHOOK_URL", "WEBHOOK_SECRET")])
def test_optional_notification_skips_without_both_settings(configured, monkeypatch, missing):
    def forbidden(*_args, **_kwargs):
        pytest.fail("network called without both settings")

    for key in missing:
        monkeypatch.delenv(key)
    monkeypatch.setattr(eval_report, "urlopen", forbidden)
    eval_report.send("report")


def test_failure_logs_no_secret_or_exception_text(configured, monkeypatch, tmp_path: Path, capsys):
    def refused(*_args, **_kwargs):
        raise URLError("private-token secret-token private evidence")

    monkeypatch.setattr(eval_report, "urlopen", refused)
    path = tmp_path / "report.md"
    path.write_text("report")
    assert eval_report.main([str(path)]) == 1
    assert capsys.readouterr().err == "::warning::campaign report notification failed\n"


def test_invalid_status_fails_before_network(configured, monkeypatch):
    monkeypatch.setenv("STATUS", "complete")
    with pytest.raises(eval_report.ValidationError):
        eval_report.send("report")
