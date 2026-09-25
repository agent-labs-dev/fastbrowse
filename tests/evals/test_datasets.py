import asyncio
import hashlib
import io
import json
import tarfile
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals import datasets


def _body() -> bytes:
    return json.dumps(
        [
            {
                "task_id": f"task-{i}",
                "website": "https://fixture.test/",
                "task_description": "Read the heading.",
                "reference_length": length,
            }
            for i, length in enumerate([1, 5, 6, 10, 11, 20])
        ]
    ).encode()


def _source(body: bytes) -> datasets.Source:
    return datasets.Source(
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


def test_mind2web_sampling_is_stratified_seeded_and_order_independent() -> None:
    tasks = datasets.online_mind2web(_body())
    assert [task.stratum for task in tasks] == ["easy", "easy", "medium", "medium", "hard", "hard"]
    chosen = datasets.sample(tasks, per_stratum=1, seed=42)
    assert len(chosen) == 3 and {task.stratum for task in chosen} == {"easy", "medium", "hard"}
    assert datasets.sample(tasks[::-1], per_stratum=1, seed=42) == chosen
    with pytest.raises(ValueError, match="needs 3"):
        datasets.sample(tasks, per_stratum=3, seed=42)
    with pytest.raises(ValueError, match="duplicate"):
        datasets.sample([*tasks, tasks[0]], per_stratum=1, seed=42)
    with pytest.raises(ValueError, match="no benchmark tasks"):
        datasets.online_mind2web(b"[]")
    assert len({tuple(t.id for t in datasets.sample(tasks, per_stratum=1, seed=seed)) for seed in range(10)}) > 1


def _archive() -> bytes:
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as archive:
        for site in ["example", "calibration-example"]:
            data = json.dumps(
                {
                    "tasks": [
                        {
                            "id": "authored-task",
                            "tier": "act-short",
                            "start_path": "/catalog",
                            "prompt": "Find {params.kind}.",
                            "params": {"kind": "a book"},
                            "predicate": {"type": "answer", "contains": ["book"]},
                        }
                    ]
                }
            ).encode()
            info = tarfile.TarInfo(f"WindTunnel-{datasets.WINDTUNNEL_REVISION}/tasks/{site}.yaml")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return stream.getvalue()


def test_windtunnel_loads_templates_without_executing_or_extracting_upstream() -> None:
    rows = datasets.windtunnel(_archive(), {"example": "http://localhost:3000"})
    assert len(rows) == 1
    assert rows[0].task == "Find a book."
    assert rows[0].start == "http://localhost:3000/catalog"
    assert rows[0].metadata["predicate"] == {"type": "answer", "contains": ["book"]}
    with pytest.raises(ValueError, match="missing site URL"):
        datasets.windtunnel(_archive(), {})
    with pytest.raises(ValueError, match="no benchmark tasks"):
        datasets.windtunnel(_archive(), {"example": "http://localhost:3000"}, revision="b" * 40)


@pytest.mark.parametrize("ending", [200, 403, 404, 503, "timeout"])
async def test_precheck_uses_recorded_redirects_errors_and_timeouts(ending: int | str) -> None:
    def recorded(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/":
            return httpx.Response(302, headers={"location": "/final"})
        if ending == "timeout":
            raise httpx.ReadTimeout("recorded", request=request)
        assert isinstance(ending, int)
        return httpx.Response(ending)

    async with httpx.AsyncClient(transport=httpx.MockTransport(recorded)) as http:
        result = await datasets.precheck(datasets.online_mind2web(_body())[0], http)
    assert result.reachable is (ending == 200)
    if ending == "timeout":
        assert result.reason == "ReadTimeout"
    else:
        assert result.final_url == "https://fixture.test/final"
        assert result.status_code == ending


def test_every_loader_has_pins_and_licence_documentation() -> None:
    docs = (Path(__file__).parents[2] / "docs" / "data-sources.md").read_text()
    for name, source in datasets.SOURCES.items():
        section = docs.split(f"## {name}\n", 1)[1].split("\n## ", 1)[0]
        assert source.upstream in section and source.revision in section
        assert (source.sha256 or "SHA-256: pending gated access") in section
        assert "Licence:" in section and "Attribution:" in section and "Fetched" in section
