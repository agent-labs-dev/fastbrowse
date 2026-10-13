"""A public-site campaign is refused before it starts in the browser those sites refuse, and nowhere else."""

import pytest

from fastbrowse.evals.live_tasks import browser_refusal


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
        None,
        {"mode": "browser-use-cloud"},
        {"mode": "local-chrome", "headed": True},
        {"mode": "local-chrome", "profile": True},
    ],
)
def test_a_headed_window_a_kept_profile_or_another_browser_may_start(browser: dict[str, object] | None) -> None:
    assert browser_refusal(browser) is None
