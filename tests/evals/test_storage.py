import base64
import hashlib
import json
import sys
import zlib
from contextlib import nullcontext
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace

import httpx
import pytest

from fastbrowse.evals import storage
from fastbrowse.evals.evidence import Campaign
from fastbrowse.evals.storage import Manifest, StoredCampaign, digest, read_campaign, verify_archive


def test_campaign_push_preserves_existing_private_archive_receipts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    manifest = tmp_path / "evidence.manifest.json"
    manifest.write_text(Manifest(campaigns=[]).model_dump_json())
    receipt_path = manifest.with_suffix(".archive.json")
    receipts = [
        {
            "path": "docs/validation/original.jsonl",
            "sha256": "a" * 64,
            "trace_id": "a" * 32,
            "bytes": 20,
            "observation_ids": ["b" * 16],
            "stored_at": "2026-10-06T10:00:00Z",
        }
    ]
    receipt_path.write_text(json.dumps(receipts))
    incoming = tmp_path / "incoming.json"
    incoming.write_text('{"schema_version":1,"campaigns":[]}')
    sdk = SimpleNamespace(flush=lambda: None, shutdown=lambda: None)
    monkeypatch.setattr(storage, "connection", lambda: nullcontext(None))
    monkeypatch.setattr(storage.importlib, "import_module", lambda _: SimpleNamespace(Langfuse=lambda **_: sdk))
    monkeypatch.setattr(storage, "read_evidence", lambda *_: storage.Evidence(campaigns=[]))
    verified = []
    monkeypatch.setattr(storage, "verify_archive", lambda _, value: verified.extend(value))
    monkeypatch.setattr(
        sys,
        "argv",
        ["storage", "--manifest", str(manifest), "--push", str(incoming), "--out", str(tmp_path / "out.json")],
    )
    storage.main()
    assert json.loads(receipt_path.read_text()) == receipts
    assert verified == receipts


def stored(content: str) -> StoredCampaign:
    return StoredCampaign(
        id="failed-run",
        sha256=digest(content),
        trace_id="a" * 32,
        observation_id="b" * 16,
        stored_at=datetime.now(UTC),
        attempts=0,
    )


def client_for(rows: list[dict]) -> httpx.Client:
    return httpx.Client(
        base_url="https://us.cloud.langfuse.com",
        transport=httpx.MockTransport(lambda request: httpx.Response(200, json={"data": rows, "meta": {}})),
    )


def test_campaign_readback_preserves_unknown_schedule_and_original_times() -> None:
    campaign = Campaign(id="failed-run", kind="diagnostic", sources=[], scheduled_slots=None, attempts=[])
    content = campaign.model_dump_json()
    entry = stored(content)
    with client_for(
        [{"id": entry.observation_id, "output": json.dumps({"content": content, "sha256": entry.sha256})}]
    ) as client:
        assert read_campaign(client, entry) == campaign


@pytest.mark.parametrize("output", [{"content": "{}"}, {"content": 4}])
def test_readback_refuses_changed_or_invalid_payload(output: dict) -> None:
    entry = stored("original")
    with (
        client_for([{"id": entry.observation_id, "output": output}]) as client,
        pytest.raises(ValueError, match="digest mismatch"),
    ):
        read_campaign(client, entry)


def test_readback_refuses_missing_observation() -> None:
    with client_for([]) as client, pytest.raises(ValueError, match="missing"):
        read_campaign(client, stored("original"))


def test_archive_verification_checks_original_bytes_across_chunks() -> None:
    original = b"failed trace\nunknown cost\n" * 100
    encoded = base64.b64encode(zlib.compress(original)).decode()
    receipt = {
        "path": "docs/validation/example.jsonl",
        "sha256": hashlib.sha256(original).hexdigest(),
        "bytes": len(original),
        "trace_id": "a" * 32,
        "stored_at": datetime.now(UTC).isoformat(),
        "observation_ids": ["first", "last"],
    }
    rows = [
        {"id": "last", "output": {"content": encoded[10:]}},
        {"id": "first", "output": {"content": encoded[:10]}},
    ]
    with client_for(rows) as client:
        verify_archive(client, [receipt])
        with pytest.raises(ValueError, match="digest mismatch"):
            verify_archive(client, [receipt | {"sha256": "0" * 64}])


def test_manifest_cannot_redirect_to_another_project() -> None:
    with pytest.raises(ValueError):
        Manifest.model_validate({"project_id": "production", "campaigns": []})


def test_resumed_archive_reuses_receipt_and_refuses_replacing_original(tmp_path: Path) -> None:
    path = tmp_path / "docs/validation/run.jsonl"
    path.parent.mkdir(parents=True)
    path.write_bytes(b"original")
    receipt = {"path": "docs/validation/run.jsonl", "sha256": hashlib.sha256(b"original").hexdigest()}
    previous = {receipt["path"]: receipt}
    assert list(storage.store_archive(None, tmp_path, previous)) == [receipt]
    path.write_bytes(b"edited")
    with pytest.raises(ValueError, match="cannot be replaced"):
        list(storage.store_archive(None, tmp_path, previous))


def test_campaign_upload_does_not_enable_public_sharing() -> None:
    def publish() -> None:
        raise AssertionError("Eval publication requires maintainer approval")

    span = SimpleNamespace(trace_id="a" * 32, id="b" * 16, set_trace_as_public=publish)
    sdk = SimpleNamespace(start_as_current_observation=lambda **_: nullcontext(span))
    campaign = Campaign(id="private-run", kind="diagnostic", sources=[], attempts=[])
    assert storage.store_campaign(sdk, campaign).id == "private-run"
