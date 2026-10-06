"""Keep eval evidence in the dedicated Langfuse project and verify it before removing local copies."""

import argparse
import base64
import hashlib
import importlib
import json
import os
import time
import zlib
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Annotated, Any, Literal

import httpx
from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter, model_validator

from fastbrowse.evals.evidence import Campaign, Digest, Evidence, Identifier

PROJECT = "cmuwjxra401iyad0cymgswes5"
HOST = "https://us.cloud.langfuse.com"


class MissingObservation(ValueError):
    """An uploaded observation has not become readable yet."""


class StoredCampaign(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: Identifier
    sha256: Digest
    trace_id: Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{32}$")]
    observation_id: Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{16}$")]
    stored_at: datetime
    attempts: Annotated[int, Field(ge=0, strict=True)]


class Manifest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    schema_version: Literal[1] = 1
    project_id: Literal["cmuwjxra401iyad0cymgswes5"] = PROJECT
    base_url: Literal["https://us.cloud.langfuse.com"] = HOST
    campaigns: list[StoredCampaign]

    @model_validator(mode="after")
    def unique_campaigns(self) -> "Manifest":
        if len({c.id for c in self.campaigns}) != len(self.campaigns):
            raise ValueError("duplicate stored campaign ids")
        for campaign in self.campaigns:
            if campaign.trace_id != campaign.sha256[:32]:
                raise ValueError("trace identity must match the stored content digest")
        return self


class ArchiveReceipt(BaseModel):
    model_config = ConfigDict(extra="forbid")
    path: Annotated[str, StringConstraints(min_length=1, max_length=1000)]
    sha256: Digest
    bytes: Annotated[int, Field(ge=0, strict=True)]
    trace_id: Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{32}$")]
    observation_ids: Annotated[list[Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{16}$")]], Field(min_length=1)]
    stored_at: datetime

    @model_validator(mode="after")
    def identity(self) -> "ArchiveReceipt":
        parts = Path(self.path).parts
        if (
            self.trace_id != self.sha256[:32]
            or ".." in parts
            or parts[:2] not in {("docs", "results"), ("docs", "validation")}
            or len(parts) < 3
        ):
            raise ValueError("invalid archived source identity")
        if len(set(self.observation_ids)) != len(self.observation_ids):
            raise ValueError("duplicate archive chunks")
        return self


def read_archive(manifest_path: Path) -> list[dict[str, Any]]:
    path = manifest_path.with_suffix(".archive.json")
    if not path.exists():
        return []
    receipts = TypeAdapter(list[ArchiveReceipt]).validate_json(path.read_text())
    if len({receipt.path for receipt in receipts}) != len(receipts):
        raise ValueError("duplicate archived source paths")
    return [receipt.model_dump(mode="json") for receipt in receipts]


def digest(content: str) -> str:
    return hashlib.sha256(content.encode()).hexdigest()


@contextmanager
def connection() -> Iterator[httpx.Client]:
    public = os.environ["LANGFUSE_PUBLIC_KEY"]
    secret = os.environ["LANGFUSE_SECRET_KEY"]
    if os.environ.get("LANGFUSE_BASE_URL", HOST).rstrip("/") != HOST:
        raise ValueError("eval storage requires the dedicated US Langfuse host")
    client = httpx.Client(base_url=HOST, auth=(public, secret), timeout=60)
    try:
        response = client.get("/api/public/projects")
        response.raise_for_status()
        projects = response.json()["data"]
        if len(projects) != 1 or projects[0]["id"] != PROJECT or projects[0]["name"] != "fastbrowse-evals":
            raise ValueError("refusing credentials outside the dedicated fastbrowse-evals project")
        yield client
    finally:
        client.close()


def observations(client: httpx.Client, trace_id: str, stored_at: datetime) -> list[dict[str, Any]]:
    params = {
        "traceId": trace_id,
        "fromStartTime": (stored_at - timedelta(minutes=5)).isoformat(),
        "fields": "core,basic,io",
        "limit": "1000",
    }
    result = []
    for _ in range(20):
        response = client.get("/api/public/v2/observations", params=params)
        response.raise_for_status()
        page = response.json()
        result.extend(page["data"])
        cursor = page["meta"].get("cursor")
        if not cursor:
            return result
        params["cursor"] = cursor
    raise ValueError("stored observations exceeded 20 pages")


def payload(row: dict[str, Any]) -> dict[str, Any]:
    value = row.get("output")
    if isinstance(value, str):
        value = json.loads(value)
    if not isinstance(value, dict):
        raise ValueError("stored evidence has no output object")
    return value


def read_campaign(client: httpx.Client, entry: StoredCampaign) -> Campaign:
    rows = observations(client, entry.trace_id, entry.stored_at)
    row = next((row for row in rows if row["id"] == entry.observation_id), None)
    if row is None:
        raise MissingObservation(f"{entry.id}: stored observation is missing")
    wrapper = payload(row)
    content = wrapper.get("content")
    if (
        set(wrapper) != {"content", "sha256"}
        or wrapper.get("sha256") != entry.sha256
        or not isinstance(content, str)
        or digest(content) != entry.sha256
    ):
        raise ValueError(f"{entry.id}: stored campaign digest mismatch")
    campaign = Campaign.model_validate_json(content)
    if campaign.id != entry.id or len(campaign.attempts) != entry.attempts:
        raise ValueError(f"{entry.id}: stored campaign identity mismatch")
    return campaign


def read_evidence(client: httpx.Client, manifest: Manifest) -> Evidence:
    return Evidence(campaigns=[read_campaign(client, entry) for entry in manifest.campaigns])


def store_campaign(sdk: Any, campaign: Campaign) -> StoredCampaign:
    content = campaign.model_dump_json()
    sha = digest(content)
    at = datetime.now(UTC)
    with sdk.start_as_current_observation(
        name=f"evidence/{campaign.id}",
        as_type="evaluator",
        trace_context={"trace_id": sha[:32]},
        output={"sha256": sha, "content": content},
        metadata={"kind": "public-eval-evidence", "recorded_times": "in campaign, storage time is not run time"},
    ) as span:
        span.set_trace_as_public()
        return StoredCampaign(
            id=campaign.id,
            sha256=sha,
            trace_id=span.trace_id,
            observation_id=span.id,
            stored_at=at,
            attempts=len(campaign.attempts),
        )


def store_archive(sdk: Any, root: Path, previous: Mapping[str, dict[str, Any]]) -> Iterator[dict[str, Any]]:
    # Compression keeps a source under the ingestion limit without dropping steps, errors or failed rows.
    for directory in (root / "docs/results", root / "docs/validation"):
        for path in sorted(directory.rglob("*")):
            if not path.is_file() or path.name in {
                "evidence.manifest.json",
                "evidence.manifest.archive.json",
                "publication-baselines.json",
            }:
                continue
            content = path.read_bytes()
            sha = hashlib.sha256(content).hexdigest()
            name = path.relative_to(root).as_posix()
            if receipt := previous.get(name):
                if receipt["sha256"] != sha:
                    raise ValueError(f"{name}: archived source cannot be replaced")
                yield receipt
                continue
            encoded = base64.b64encode(zlib.compress(content)).decode()
            chunks = [encoded[i : i + 60000] for i in range(0, len(encoded), 60000)] or [""]
            at = datetime.now(UTC)
            ids = []
            for index, chunk in enumerate(chunks):
                with sdk.start_as_current_observation(
                    name=f"validation/{path.name}/{index}",
                    trace_context={"trace_id": sha[:32]},
                    output={"encoding": "zlib-base64", "chunk": index, "content": chunk},
                    metadata={"kind": "private-validation-source", "sha256": sha},
                ) as span:
                    ids.append(span.id)
            yield {
                "path": name,
                "sha256": sha,
                "bytes": len(content),
                "trace_id": sha[:32],
                "observation_ids": ids,
                "stored_at": at.isoformat(),
            }


def verify_archive(client: httpx.Client, receipts: list[dict[str, Any]]) -> None:
    for receipt in receipts:
        rows = observations(client, receipt["trace_id"], datetime.fromisoformat(receipt["stored_at"]))
        indexed = {row["id"]: row for row in rows}
        chunks = [payload(indexed[oid])["content"] for oid in receipt["observation_ids"]]
        content = zlib.decompress(base64.b64decode("".join(chunks), validate=True))
        if len(content) != receipt["bytes"] or hashlib.sha256(content).hexdigest() != receipt["sha256"]:
            raise ValueError(f"{receipt['path']}: archived source digest mismatch")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--check", action="store_true", help="check the manifest without service credentials")
    parser.add_argument("--push", type=Path, help="sanitized Evidence JSON to upload")
    parser.add_argument("--archive-root", type=Path)
    args = parser.parse_args()
    if args.check:
        Manifest.model_validate_json(args.manifest.read_text())
        read_archive(args.manifest)
        return
    if args.out is None:
        parser.error("--out is required for uploading or reading stored evidence")
    with connection() as client:
        receipt_path = args.manifest.with_suffix(".archive.json")
        receipts = read_archive(args.manifest)
        if args.push:
            evidence = Evidence.model_validate_json(args.push.read_text())
            previous = (
                {entry.id: entry for entry in Manifest.model_validate_json(args.manifest.read_text()).campaigns}
                if args.manifest.exists()
                else {}
            )
            sdk = importlib.import_module("langfuse").Langfuse(base_url=HOST)
            try:
                entries = dict(previous)
                for campaign in evidence.campaigns:
                    entry = previous.get(campaign.id)
                    if entry is not None and entry.sha256 == digest(campaign.model_dump_json()):
                        read_campaign(client, entry)
                        entries[campaign.id] = entry
                    else:
                        entries[campaign.id] = store_campaign(sdk, campaign)
                        sdk.flush()
                        args.manifest.write_text(
                            Manifest(campaigns=list(entries.values())).model_dump_json(indent=2) + "\n"
                        )
                manifest = Manifest(campaigns=list(entries.values()))
                if args.archive_root:
                    archived = {receipt["path"]: receipt for receipt in receipts}
                    for receipt in store_archive(sdk, args.archive_root, archived):
                        archived[receipt["path"]] = receipt
                        sdk.flush()
                        receipt_path.write_text(json.dumps(list(archived.values()), indent=2) + "\n")
                    receipts = list(archived.values())
                sdk.flush()
            finally:
                sdk.shutdown()
            # Keep the upload receipt even if ingestion has not become readable yet.
            args.manifest.write_text(manifest.model_dump_json(indent=2) + "\n")
            receipt_path.write_text(json.dumps(receipts, indent=2) + "\n")
        else:
            manifest = Manifest.model_validate_json(args.manifest.read_text())
        for attempt in range(12):
            try:
                evidence = read_evidence(client, manifest)
                verify_archive(client, receipts)
                break
            except (MissingObservation, KeyError):
                if attempt == 11:
                    raise
                time.sleep(5)
            except httpx.HTTPStatusError as exc:
                if exc.response.status_code not in {404, 429, 500, 502, 503, 504} or attempt == 11:
                    raise
                time.sleep(5)
            except httpx.TransportError:
                if attempt == 11:
                    raise
                time.sleep(5)
        args.out.write_text(evidence.model_dump_json(indent=2) + "\n")
        print(json.dumps({"verified_campaigns": len(evidence.campaigns), "verified_sources": len(receipts)}))


if __name__ == "__main__":
    main()
