"""Pinned external task loaders and HTTP reachability checks, with no agent or judge calls."""

import argparse
import asyncio
import hashlib
import io
import json
import os
import random
import re
import sys
import tarfile
import tempfile
from collections import defaultdict
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Literal
from urllib.parse import urljoin, urlparse

import httpx
from pydantic import BaseModel, ConfigDict, Field, JsonValue, TypeAdapter


class Source(BaseModel):
    model_config = ConfigDict(frozen=True)

    id: Literal["online-mind2web", "windtunnel"]
    upstream: str
    revision: str = Field(pattern=r"^[a-f0-9]{40}$")
    url: str
    sha256: str | None = Field(default=None, pattern=r"^[a-f0-9]{64}$")
    git_blob: str | None = None


OM2W_REVISION = "eacad896a84dc5b65e29b0b06e4699ab0544d701"
WINDTUNNEL_REVISION = "5ca8644e23826ebb30108e7bad240b61043bfe67"
SOURCES = {
    "online-mind2web": Source(
        id="online-mind2web",
        upstream="https://huggingface.co/datasets/osunlp/Online-Mind2Web",
        revision=OM2W_REVISION,
        url=f"https://huggingface.co/datasets/osunlp/Online-Mind2Web/resolve/{OM2W_REVISION}/Online_Mind2Web.json",
        git_blob="e8a5e5a99f2be9eae14f4e4259bd5af562f80da9",
    ),
    "windtunnel": Source(
        id="windtunnel",
        upstream="https://github.com/nekuda-ai/WindTunnel",
        revision=WINDTUNNEL_REVISION,
        url=f"https://codeload.github.com/nekuda-ai/WindTunnel/tar.gz/{WINDTUNNEL_REVISION}",
        sha256="9254737b8062a4a7140ec2a91f3b111abe648bd6d649b2775d2e68ad1f054d82",
    ),
}


class ExternalTask(BaseModel):
    source: Literal["online-mind2web", "windtunnel"]
    id: str
    start: str
    task: str
    stratum: Literal["easy", "medium", "hard", "answer", "act-short", "act-long", "transaction"]
    site: str | None = None
    metadata: dict[str, JsonValue] = {}


def cache_dir() -> Path:
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "fastbrowse" / "evals"


def _verify(source: Source, body: bytes) -> None:
    actual = hashlib.sha256(body).hexdigest()
    if actual != source.sha256:
        raise ValueError(f"{source.id}: sha256 mismatch: expected {source.sha256}, got {actual}")
    if source.git_blob:
        blob = hashlib.sha1(f"blob {len(body)}\0".encode() + body).hexdigest()
        if blob != source.git_blob:
            raise ValueError(f"{source.id}: file does not match the pinned upstream git blob")


async def fetch(
    source: Source, http: httpx.AsyncClient, *, cache: Path | None = None, token: str | None = None
) -> bytes:
    if source.sha256 is None:
        raise ValueError(
            f"{source.id}: a verified sha256 is required for revision {source.revision}; see docs/data-sources.md"
        )
    folder = cache_dir() if cache is None else cache
    target = folder / f"{source.id}-{source.revision}-{source.sha256}"
    if target.exists():
        body = target.read_bytes()
        _verify(source, body)
        return body
    headers = {"Authorization": f"Bearer {token}"} if token and source.id == "online-mind2web" else {}
    response = await http.get(source.url, headers=headers, follow_redirects=True)
    if response.status_code in (401, 403):
        raise ValueError(f"{source.id}: access denied; accept the upstream terms and configure HF_TOKEN")
    response.raise_for_status()
    body = response.content
    _verify(source, body)
    folder.mkdir(parents=True, exist_ok=True)
    # A reader must never see a half-written cache file from a concurrent download.
    with tempfile.NamedTemporaryFile(dir=folder, delete=False) as temporary:
        temporary.write(body)
    await asyncio.to_thread(Path(temporary.name).replace, target)
    return body


class _Mind2WebRow(BaseModel):
    task_id: str
    website: str
    task_description: str
    reference_length: int = Field(gt=0)


def online_mind2web(body: bytes) -> list[ExternalTask]:
    rows = TypeAdapter(list[_Mind2WebRow]).validate_json(body)
    return _unique(
        [
            ExternalTask(
                source="online-mind2web",
                id=row.task_id,
                start=_http_url(row.website),
                task=row.task_description,
                stratum="easy" if row.reference_length <= 5 else "medium" if row.reference_length <= 10 else "hard",
                metadata={"reference_length": row.reference_length},
            )
            for row in rows
        ]
    )


def _http_url(value: str) -> str:
    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError(f"expected an HTTP URL without credentials: {value!r}")
    return value


class _WindTask(BaseModel):
    id: str
    tier: Literal["answer", "act-short", "act-long", "transaction"]
    prompt: str
    start_path: str = "/"
    params: dict[str, JsonValue] = {}
    predicate: dict[str, JsonValue]
    max_steps: dict[str, int] = {}


class _WindFile(BaseModel):
    tasks: list[_WindTask]


def windtunnel(body: bytes, site_urls: Mapping[str, str], *, revision: str = WINDTUNNEL_REVISION) -> list[ExternalTask]:
    import yaml  # the eval-data extra; no upstream code is executed

    tasks = []
    prefix = f"WindTunnel-{revision}/tasks/"
    with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as archive:
        for member in sorted(archive.getmembers(), key=lambda m: m.name):
            if not member.name.startswith(prefix) or not member.name.endswith(".yaml") or not member.isfile():
                continue
            site = member.name.removeprefix(prefix).removesuffix(".yaml")
            if site.startswith("calibration-"):
                continue
            if "/" in site or site not in site_urls:
                raise ValueError(f"missing site URL for {site}")
            base = _http_url(site_urls[site])
            stream = archive.extractfile(member)
            assert stream is not None
            for row in _WindFile.model_validate(yaml.safe_load(stream)).tasks:
                prompt = re.sub(r"\{params\.([^}]+)\}", lambda m, row=row: str(row.params[m[1]]), row.prompt)
                start = urljoin(base.rstrip("/") + "/", row.start_path.lstrip("/"))
                if urlparse(start).netloc != urlparse(base).netloc:
                    raise ValueError(f"{row.id}: start_path leaves its site")
                tasks.append(
                    ExternalTask(
                        source="windtunnel",
                        id=row.id,
                        start=start,
                        task=prompt,
                        stratum=row.tier,
                        site=site,
                        metadata={"predicate": row.predicate, "params": row.params, "max_steps": row.max_steps},
                    )
                )
    if not tasks:
        raise ValueError("windtunnel: archive contains no benchmark tasks at the pinned path")
    return _unique(tasks)


def _unique(tasks: list[ExternalTask]) -> list[ExternalTask]:
    if not tasks:
        raise ValueError("no benchmark tasks")
    if len({task.id for task in tasks}) != len(tasks):
        raise ValueError("duplicate task ids")
    return tasks


def sample(tasks: Sequence[ExternalTask], *, per_stratum: int, seed: int) -> list[ExternalTask]:
    """Equal allocation in stable seeded order; reject undersized strata rather than silently changing the draw."""
    if per_stratum < 1:
        raise ValueError("per_stratum must be positive")
    _unique(list(tasks))
    groups: dict[str, list[ExternalTask]] = defaultdict(list)
    for task in tasks:
        groups[task.stratum].append(task)
    rng = random.Random(seed)
    result = []
    for name, group in sorted(groups.items()):
        if len(group) < per_stratum:
            raise ValueError(f"stratum {name} has {len(group)} tasks, needs {per_stratum}")
        ordered = sorted(group, key=lambda task: task.id)
        rng.shuffle(ordered)
        result.extend(ordered[:per_stratum])
    return result


class Reachability(BaseModel):
    task: str
    start: str
    final_url: str | None = None
    status_code: int | None = None
    reachable: bool
    reason: str


async def precheck(task: ExternalTask, http: httpx.AsyncClient) -> Reachability:
    """An HTTP load check, not a browser feasibility or CAPTCHA judgement; injectable transport makes it offline."""
    try:
        response = await http.get(_http_url(task.start), follow_redirects=True)
    except httpx.HTTPError as exc:
        return Reachability(task=task.id, start=task.start, reachable=False, reason=type(exc).__name__)
    return Reachability(
        task=task.id,
        start=task.start,
        final_url=str(response.url),
        status_code=response.status_code,
        reachable=response.is_success,
        reason="http_ok" if response.is_success else f"http_{response.status_code}",
    )


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", choices=list(SOURCES))
    parser.add_argument("--cache", type=Path)
    parser.add_argument("--sha256", help="verified digest for the pinned gated Online-Mind2Web file")
    parser.add_argument("--site-urls", type=Path, help="WindTunnel site id to local URL, as JSON")
    parser.add_argument("--per-stratum", type=int)
    parser.add_argument("--seed", type=int, default=20260925)
    parser.add_argument("--precheck", action="store_true", help="HTTP reachability only, no agent")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    source = SOURCES[args.source]
    if args.sha256:
        if source.id != "online-mind2web":
            parser.error("--sha256 is only for the gated Online-Mind2Web pin")
        source = Source.model_validate(source.model_dump() | {"sha256": args.sha256})
    try:
        async with httpx.AsyncClient(timeout=30) as http:
            body = await fetch(source, http, cache=args.cache, token=os.environ.get("HF_TOKEN"))
            if source.id == "windtunnel":
                if args.site_urls is None:
                    parser.error("windtunnel requires --site-urls")
                tasks = windtunnel(body, json.loads(args.site_urls.read_text()))
            else:
                tasks = online_mind2web(body)
            if args.per_stratum is not None:
                tasks = sample(tasks, per_stratum=args.per_stratum, seed=args.seed)
            rows = []
            for task in tasks:
                reachability = (await precheck(task, http)).model_dump() if args.precheck else None
                rows.append(
                    {
                        "source_revision": source.revision,
                        "source_sha256": source.sha256,
                        "seed": args.seed,
                        "task": task.model_dump(),
                        "reachability": reachability,
                    }
                )
    except (ValueError, httpx.HTTPError) as exc:
        parser.error(str(exc))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in rows), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main(sys.argv[1:])))
