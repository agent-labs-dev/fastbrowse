import json

import pytest

from fastbrowse.browser.page import _capped, _same_http_origin
from fastbrowse.models import Operation
from fastbrowse.page import Control, cut_text, pager_link, pages_forward


def link(label: str, *, next_page: bool | None = None, i: int = 0) -> Control:
    return Control(
        id=f"l{i}",
        frame_id=None,
        role="link",
        label=label,
        operations=frozenset({Operation.CLICK}),
        href=f"/{i}",
        next_page=next_page,
    )


@pytest.mark.parametrize(
    "label", ["next", "Next", "Next page", "Next \u203a", "\u203a", "\u00bb", "  next\n", "Older posts", "More results"]
)
def test_labels_that_turn_the_page(label: str) -> None:
    assert pages_forward(link(label))


@pytest.mark.parametrize("label", ["Nextel", "Book 3", "previous", "Next steps for the team", "Home"])
def test_labels_that_do_not(label: str) -> None:
    assert not pages_forward(link(label))


def test_a_link_the_page_marks_rel_next_turns_the_page_whatever_it_says() -> None:
    assert pages_forward(link("Suivant", next_page=True))


def test_the_cap_keeps_the_first_controls_and_every_pager() -> None:
    controls = [link(f"Book {i}", i=i) for i in range(10)] + [link("next", i=10)]
    kept = _capped(controls, 4)
    assert [c.label for c in kept] == ["Book 0", "Book 1", "Book 2", "next"]


def test_the_cap_leaves_a_short_list_alone() -> None:
    controls = [link(f"Book {i}", i=i) for i in range(3)]
    assert _capped(controls, 5) == controls


def test_the_cap_keeps_document_order_and_never_returns_more_than_the_limit() -> None:
    controls = [link("next", i=0), *(link(f"Book {i}", i=i) for i in range(1, 6)), link("next", i=6)]
    assert [c.id for c in _capped(controls, 3)] == ["l0", "l1", "l6"]
    # A page drawing more pagers than the cap allows is still answered within the cap.
    assert [c.id for c in _capped([link("next", i=i) for i in range(5)], 2)] == ["l0", "l1"]


def test_only_a_link_turns_the_page_so_only_a_link_is_exempt_from_the_cap() -> None:
    # A carousel's arrow and a wizard's button read exactly like a pager; neither is ever followed, so neither
    # takes a place in the observation from a control that would have been offered.
    carousels = [link("next", i=i).model_copy(update={"role": "button", "href": None}) for i in range(100, 108)]
    controls = [*(link(f"Book {i}", i=i) for i in range(6)), *carousels, link("next", i=20)]
    kept = _capped(controls, 4)
    assert [c.id for c in kept] == ["l0", "l1", "l2", "l20"]


def test_a_calendar_next_without_a_destination_is_not_a_pager() -> None:
    # A jQuery UI datepicker's Next is a hrefless anchor snapshot.js now offers as a button, not a link, so it
    # never competes with the list's own pager for the cap or a "keep going" choice.
    assert not pager_link(link("Next").model_copy(update={"role": "button", "href": None}))


def test_a_control_that_only_looks_like_a_pager_is_not_one() -> None:
    assert not pager_link(link("next").model_copy(update={"role": "button"}))
    assert not pager_link(link("next").model_copy(update={"href": None}))
    assert not pager_link(link("next").model_copy(update={"operations": frozenset()}))
    assert pager_link(link("next"))


def test_a_wizard_with_no_earlier_entry_but_blank_cannot_go_back() -> None:
    assert not _same_http_origin("about:blank", "https://example.test/wizard")


def test_a_cross_origin_predecessor_cannot_go_back() -> None:
    assert not _same_http_origin("https://other.test/", "https://example.test/")
    # Same host, different scheme or port is a different origin too.
    assert not _same_http_origin("http://example.test/", "https://example.test/")
    assert not _same_http_origin("https://example.test:8443/", "https://example.test/")


def test_an_empty_predecessor_cannot_go_back() -> None:
    assert not _same_http_origin("", "https://example.test/")


def test_a_same_origin_predecessor_can_go_back() -> None:
    assert _same_http_origin("https://example.test/start", "https://example.test/next")
    # The default port for the scheme is the effective one when neither writes it out.
    assert _same_http_origin("https://example.test:443/start", "https://example.test/next")


def test_escaped_text_keeps_every_character_that_fits() -> None:
    # Each character of this costs six once JSON-escaped; subtracting the escaped excess from a count of
    # source characters cut it to nothing although nine hundred of them fit.
    text = "東京" * 1000
    cut = cut_text(text, 6000, json_encoded=True)
    size = len(json.dumps(cut)) - 2
    assert size <= 6000
    assert cut.startswith("東京" * 450) and "characters omitted" in cut
