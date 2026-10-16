"""A public-site campaign starts only in a browser it records and those sites accept; anything else is refused."""

import pytest

from fastbrowse.evals.live_tasks import browser_refusal, run_browser


@pytest.mark.parametrize(
    "browser",
    [
        {"mode": "local-chrome"},
        {"mode": "local-chrome", "headed": False},
        {"mode": "local-chrome", "headed": False, "profile": False},
    ],
)
def test_headless_chrome_with_a_throwaway_profile_is_refused(browser: dict[str, object]) -> None:
    assert browser_refusal(browser) is not None


@pytest.mark.parametrize(
    "browser",
    [
        {"mode": "browser-use-cloud"},
        {"mode": "browser-use-hosted"},
        {"mode": "fastbrowse-pool", "host": "pool.internal"},
        {"mode": "local-chrome", "headed": True},
        {"mode": "local-chrome", "profile": True},
    ],
)
def test_a_headed_window_a_kept_profile_or_a_cloud_browser_may_start(browser: dict[str, object]) -> None:
    assert browser_refusal(browser) is None


@pytest.mark.parametrize(
    "browser",
    [
        None,
        "local-chrome",
        {},
        {"mode": "some-other-browser"},
        {"mode": "local-chrome", "headed": "yes"},
        {"mode": "local-chrome", "profile": "/home/someone/profile"},
    ],
)
def test_a_missing_malformed_or_unknown_browser_record_is_refused(browser: object) -> None:
    assert browser_refusal(browser) is not None


def test_each_arm_is_judged_by_the_browser_it_ran_in() -> None:
    """A campaign records one browser per arm; an arm without a record is refused, not read as another arm's."""
    run = {"browsers": {"fastbrowse": {"mode": "browser-use-cloud"}, "fastbrowse-pool": {"mode": "local-chrome"}}}
    assert browser_refusal(run_browser(run, "fastbrowse")) is None
    assert browser_refusal(run_browser(run, "fastbrowse-pool")) is not None
    assert browser_refusal(run_browser(run, "browser-use")) is not None
    assert run_browser({"browser": {"mode": "browser-use-cloud"}}, "browser-use") == {"mode": "browser-use-cloud"}
