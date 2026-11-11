"""Diagnostic tasks from a maintainer's weekly sweep of their WoW addons' store pages, outside the catalog
(`live.DIAGNOSTIC_SUITES`): run to see where each agent gets stuck, never versioned or published."""

import html
import json
import re

import httpx
from pydantic import BaseModel

from fastbrowse_evals.live_tasks import Category, LiveTask, Outcome
from fastbrowse_evals.more_tasks import _fixed, _has, _json

# The project page and comments on CurseForge, and the listing on Wago Addons. CurseForge sits behind a
# Cloudflare check that clears for a browser that waits.
SWEEP_ADDON = "shortest-path-forever"
SWEEP_CURSEFORGE_ID = 1708599
SWEEP_HEADINGS = ("Features", "Usage", "The Forever addons")
SWEEP_CAPTIONS = (
    "A route settling, the departure countdown and compass",
    "Journey steps with time and distance remaining",
    "Route and waypoint on the minimap",
    "The Nearby services tab in the world map's quest log",
    "Dock destinations and departures",
)
"""The project page as `docs/curseforge.md` in cjber/shortest-path-forever publishes it; CurseForge's project API
refuses anonymous reads, so this truth is fixed and moves with that file."""


async def _curseforge_newest(http: httpx.AsyncClient) -> object:
    body = await _json(
        http, f"https://www.curseforge.com/api/v1/mods/{SWEEP_CURSEFORGE_ID}/comments?page=0&pageSize=20"
    )
    assert isinstance(body, dict)
    comments = sorted(
        (c for c in body["data"] if str(c.get("isPinned")).lower() != "true"),
        key=lambda c: int(c["datePosted"]),
        reverse=True,
    )
    # The page names a commenter by display name; `username` is the account login behind it.
    return [c["author"]["displayName"] for c in comments[:5]]


def _curseforge_comments(outcome: Outcome, truth: object) -> str | None:
    assert isinstance(truth, list)
    # A comment posted during the run pushes the fifth newest out of the list the agent saw, so the four newest at
    # the start are the ones every correct answer names.
    return _has(*truth[:4])(outcome, truth)


class WagoListing(BaseModel):
    description: str
    gallery_images: int


async def _wago_listing(http: httpx.AsyncClient) -> object:
    """The listing as Wago's own pages carry it: the Inertia `data-page` JSON of the addon and its gallery pages."""

    async def page(path: str) -> dict[str, object]:
        response = await http.get(f"https://addons.wago.io/addons/{SWEEP_ADDON}{path}")
        response.raise_for_status()
        found = re.search(r'data-page="([^"]+)"', response.text)
        assert found, f"no data-page on {path or '/'}"
        return json.loads(html.unescape(found.group(1)))["props"]

    description = str((await page(""))["description"])
    gallery = (await page("/gallery"))["images"]
    assert isinstance(gallery, dict)
    return {"description": description, "gallery_images": int(gallery["total"] or 0)}


def _wago_check(outcome: Outcome, truth: object) -> str | None:
    data = outcome.data
    if not isinstance(data, dict):
        return f"no structured data: {data!r}"
    assert isinstance(truth, dict)
    # Whitespace is layout: Wago trims and wraps the text it shows.
    words = [" ".join(str(d.get("description", "")).split()) for d in (data, truth)]
    if words[0] != words[1]:
        return f"description {data.get('description')!r}, expected {truth['description']!r}"
    if data.get("gallery_images") != truth["gallery_images"]:
        return f"gallery_images {data.get('gallery_images')!r}, expected {truth['gallery_images']}"
    return None


SWEEP: tuple[LiveTask, ...] = (
    LiveTask(
        "sweep-curseforge-page",
        f"https://www.curseforge.com/wow/addons/{SWEEP_ADDON}",
        "Read this addon's description and its Gallery tab. List every heading in the description in order, then "
        "every image in the Gallery tab in order with its title.",
        _fixed(None),
        _has(*SWEEP_HEADINGS, *SWEEP_CAPTIONS),
        Category.LOOKUP,
    ),
    LiveTask(
        "sweep-curseforge-comments",
        f"https://www.curseforge.com/wow/addons/{SWEEP_ADDON}/comments",
        "List the 5 newest top-level comments: each commenter's username, the date, the comment text, and whether "
        "the author cjber replied.",
        _curseforge_newest,
        _curseforge_comments,
        Category.LOOKUP,
    ),
    LiveTask(
        "sweep-wago-listing",
        f"https://addons.wago.io/addons/{SWEEP_ADDON}",
        "Report this addon's full description text exactly, and how many images its Gallery holds across all "
        "gallery pages.",
        _wago_listing,
        _wago_check,
        Category.LOOKUP,
        output_schema=WagoListing,
    ),
)
