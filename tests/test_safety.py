import pytest

from fastbrowse.models import Operation, SecretRef
from fastbrowse.page import Control
from fastbrowse.safety import Redactor, may_be_irreversible, origin_of, secret_allowed


def control(label: str, *, role: str = "button", href: str | None = None, input_type: str | None = None) -> Control:
    return Control(
        id="c",
        frame_id=None,
        role=role,
        label=label,
        operations=frozenset({Operation.CLICK}),
        href=href,
        input_type=input_type,
    )


@pytest.mark.parametrize(
    ("target", "asked"),
    [
        # No keyword and no submit type: a button is still asked about, because its script can commit anything.
        (control("Place your order"), True),
        (control("Erase"), True),
        (control("Next"), True),
        (control("Search", input_type="submit"), True),
        (control("Delete account", role="link", href="/account/delete"), True),
        (control("Documentation", role="link", href="/docs"), False),
    ],
)
def test_every_button_is_asked_about_and_plain_navigation_is_not(target: Control, asked: bool) -> None:
    assert may_be_irreversible(Operation.CLICK, target) is asked


def test_a_secret_is_caught_in_every_encoding_a_page_or_log_carries_it_in() -> None:
    redactor = Redactor()
    redactor.register("password", 'p@ss w"ord')
    for text in ('p@ss w"ord', "p%40ss%20w%22ord", "p%40ss+w%22ord", '{"v": "p@ss w\\"ord"}'):
        assert redactor.reveals(text), text
        assert "ss" not in redactor.redact(text).replace("[secret:password]", ""), text


@pytest.mark.parametrize(
    ("url", "origin"),
    [
        ("https://shop.example.test:443/cart", "https://shop.example.test"),
        ("http://shop.example.test:80/", "http://shop.example.test"),
        ("https://SHOP.example.test/", "https://shop.example.test"),
        ("https://shop.example.test:8443/", "https://shop.example.test:8443"),
        ("http://shop.example.test:443/", "http://shop.example.test:443"),
        ("https://user:pw@shop.example.test/", "https://shop.example.test"),
    ],
)
def test_a_port_the_scheme_implies_is_not_part_of_the_origin(url: str, origin: str) -> None:
    assert origin_of(url) == origin


def test_a_secret_declared_without_the_port_is_typed_on_the_url_that_carries_it() -> None:
    ref = SecretRef(name="password", origins=("https://shop.example.test",))
    assert secret_allowed(ref, origin_of("https://shop.example.test:443/login"))
    assert not secret_allowed(ref, origin_of("https://shop.example.test:8443/login"))
