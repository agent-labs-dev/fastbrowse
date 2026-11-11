import html
import json

import httpx

from fastbrowse_evals import catalog, live, sweep_tasks
from fastbrowse_evals.live_tasks import Outcome

STUB = "Shortest Path Forever for WoW: Forever.\n\nSource and issues: https://github.com/cjber/shortest-path-forever"


def page(props: dict[str, object]) -> str:
    return f'<div id="app" data-page="{html.escape(json.dumps({"props": props}))}"></div>'


async def test_the_wago_truth_is_read_from_the_listing_and_its_gallery_pages() -> None:
    pages = {
        "/addons/shortest-path-forever": page({"description": STUB}),
        "/addons/shortest-path-forever/gallery": page({"images": {"total": 0, "data": []}}),
    }
    transport = httpx.MockTransport(lambda request: httpx.Response(200, text=pages[request.url.path]))
    async with httpx.AsyncClient(transport=transport) as http:
        truth = await sweep_tasks._wago_listing(http)
    assert truth == {"description": STUB, "gallery_images": 0}
    read = {"description": " ".join(STUB.split()), "gallery_images": 0}
    assert sweep_tasks._wago_check(Outcome(None, read, None), truth) is None
    wrong = sweep_tasks._wago_check(Outcome(None, {**read, "gallery_images": 3}, None), truth)
    assert wrong is not None and "gallery_images 3" in wrong


async def test_the_comment_truth_is_the_newest_unpinned_comments() -> None:
    comments = [
        {
            "author": {"username": f"login{n}", "displayName": f"user{n}"},
            "datePosted": str(1000 + n),
            "isPinned": "False",
        }
        for n in range(7)
    ]
    comments.append(
        {"author": {"username": "pinned", "displayName": "pinned"}, "datePosted": "9999", "isPinned": "True"}
    )
    transport = httpx.MockTransport(lambda _: httpx.Response(200, json={"data": comments}))
    async with httpx.AsyncClient(transport=transport) as http:
        truth = await sweep_tasks._curseforge_newest(http)
    assert truth == ["user6", "user5", "user4", "user3", "user2"]
    # The fifth may be pushed out by a comment posted during the run; the four newest may not.
    assert sweep_tasks._curseforge_comments(Outcome("user6 user5 user4 user3 late", None, None), truth) is None
    assert sweep_tasks._curseforge_comments(Outcome("user6 user5 user4", None, None), truth)


def test_the_sweep_suite_runs_but_stays_out_of_the_catalog() -> None:
    assert live.RUNNABLE["sweep"] == sweep_tasks.SWEEP
    assert "sweep" not in live.SUITES
    assert not {t.id for t in sweep_tasks.SWEEP} & {
        t["id"] for s in json.loads(catalog.render())["suites"].values() for t in s
    }
