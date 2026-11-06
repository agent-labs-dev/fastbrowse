"""Fetch a pinned external task file and check it against its recorded digest and upstream Git blob."""

import asyncio
import hashlib
import os
import tempfile
from pathlib import Path

import httpx
from fastbrowse.evals.sources import Source


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
            f"{source.id}: a verified sha256 is required for revision {source.revision}; see evals/THIRD_PARTY_DATA.md"
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
