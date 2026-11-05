import asyncio
import hashlib
import json
from pathlib import Path

import httpx
import pytest
from fastbrowse.evals.sources import Source

from fastbrowse_evals import datasets


def _body() -> bytes:
    # The keys of the pinned Online_Mind2Web.json rows, read from the file at revision eacad896.
    return json.dumps(
        [
            {
                "task_id": f"task-{i}",
                "website": "https://fixture.test/",
                "confirmed_task": "Read the heading.",
                "reference_length": length,
                "level": "easy",
            }
            for i, length in enumerate([1, 5, 6, 10, 11, 20])
        ]
    ).encode()


def _source(body: bytes) -> Source:
    return Source(
        id="online-mind2web",
        upstream="https://fixture.test",
        revision="a" * 40,
        url="https://fixture.test/data",
        sha256=hashlib.sha256(body).hexdigest(),
    )


async def test_verified_cache_never_accepts_changed_bytes(tmp_path: Path) -> None:
    body = _body()
    requests = []

    def recorded(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(200, content=body)

    async with httpx.AsyncClient(transport=httpx.MockTransport(recorded)) as http:
        assert await datasets.fetch(_source(body), http, cache=tmp_path) == body
        assert await datasets.fetch(_source(body), http, cache=tmp_path) == body
        assert len(requests) == 1
        await asyncio.to_thread(lambda: next(tmp_path.iterdir()).write_bytes(b"corrupt"))
        with pytest.raises(ValueError, match="sha256 mismatch"):
            await datasets.fetch(_source(body), http, cache=tmp_path)
        assert len(requests) == 1


async def test_bad_download_and_gating_leave_no_cache(tmp_path: Path) -> None:
    for status, content, error in [(200, b"changed", "sha256 mismatch"), (401, b"denied", "access denied")]:
        async with httpx.AsyncClient(
            transport=httpx.MockTransport(
                lambda _, status=status, content=content: httpx.Response(status, content=content)
            )
        ) as http:
            with pytest.raises(ValueError, match=error):
                await datasets.fetch(_source(_body()), http, cache=tmp_path)
    assert not await asyncio.to_thread(lambda: list(tmp_path.iterdir()))


async def test_unverified_gated_pin_fails_before_network(tmp_path: Path) -> None:
    def network(_: httpx.Request) -> httpx.Response:
        raise AssertionError("no request without a verified digest")

    async with httpx.AsyncClient(transport=httpx.MockTransport(network)) as http:
        with pytest.raises(ValueError, match="verified sha256"):
            await datasets.fetch(_source(_body()).model_copy(update={"sha256": None}), http, cache=tmp_path)
