"""A public-site campaign starts only in a browser it records and those sites accept; anything else is refused."""

import pytest

from fastbrowse.evals.live_tasks import arm_browser_refusal, browser_refusal


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


CLOUD = {"mode": "browser-use-cloud"}
POOL = {"mode": "fastbrowse-pool", "host": "pool.internal"}
HOSTED = {"mode": "browser-use-hosted"}


@pytest.mark.parametrize(
    "arm,browser",
    [
        ("fastbrowse", CLOUD),
        ("fastbrowse", {"mode": "local-chrome", "headed": True}),
        ("fastbrowse-pool", POOL),
        ("jev-ultrafast", CLOUD),
        ("browser-use", HOSTED),
    ],
)
def test_each_arm_may_publish_from_the_browser_it_is_measured_in(arm: str, browser: dict[str, object]) -> None:
    assert arm_browser_refusal({"browsers": {arm: browser}}, arm) is None


@pytest.mark.parametrize(
    "arm,browser",
    [
        ("fastbrowse-pool", CLOUD),  # a swapped record would publish the cloud browser as the pool
        ("fastbrowse", POOL),
        ("browser-use", CLOUD),
        ("jev-ultrafast", HOSTED),
        ("fastbrowse", {"mode": "local-chrome"}),  # still headless with a throwaway profile
        ("browser-use-oss", CLOUD),  # an arm this build does not know
        ("fastbrowse", None),
    ],
)
def test_an_arm_in_a_browser_it_is_not_measured_in_is_refused(arm: str, browser: object) -> None:
    assert arm_browser_refusal({"browsers": {arm: browser}}, arm) is not None


def test_an_arm_is_never_judged_by_another_arms_browser() -> None:
    assert arm_browser_refusal({"browsers": {"fastbrowse": CLOUD}}, "fastbrowse-pool") is not None


def test_an_older_campaigns_single_browser_is_judged_as_before() -> None:
    assert arm_browser_refusal({"browser": CLOUD}, "browser-use") is None
    assert arm_browser_refusal({"browser": POOL}, "fastbrowse-pool") is not None
