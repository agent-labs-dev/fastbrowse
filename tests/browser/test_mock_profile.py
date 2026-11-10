"""The profile fixture on a real browser: its Save sends the profile once every required field holds a value."""

import pytest

from fastbrowse import run_task
from fastbrowse.evals.mock import mock_site
from tests.scripted import ScriptedJev, ScriptedLLM, steps


@pytest.mark.timeout(120)
async def test_the_profile_fixture_saves_once_its_required_country_is_chosen(cdp_url: str) -> None:
    # Option 1 is France, after the "Choose a country" placeholder.
    with mock_site() as (base, site):
        result = await run_task(
            "Set the country to France and save the profile",
            start=f"{base}/profile",
            cdp_url=cdp_url,
            jev=ScriptedJev(steps("select Country *=1; click Save")),
            llm=ScriptedLLM(),
        )
        assert [step.target for step in result.steps][:2] == ["Country *", "Save"], result.steps
        assert ("/profile", {"name": "Ada", "country": "France"}) in site.posts, site.posts
