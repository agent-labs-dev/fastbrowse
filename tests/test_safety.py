import pytest

from fastbrowse.models import Operation, SecretRef
from fastbrowse.page import Control
from fastbrowse.safety import Redactor, ScopedSecrets, may_be_irreversible, origin_of, secret_allowed


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
    "target",
    [
        control("Place your order"),
        control("Yes, I'm sure", role="link", href="/unsubscribe?token=1"),
        # A message box with no form sends on Enter through the page's own script.
        control("Send message", role="textbox"),
        control("Search", input_type="submit"),
    ],
)
def test_every_click_and_every_enter_is_asked_about(target: Control) -> None:
    assert may_be_irreversible(Operation.CLICK, target) is True
    assert may_be_irreversible(Operation.ENTER, target) is True
    assert may_be_irreversible(Operation.FILL, target) is False


def test_a_secret_is_caught_in_every_encoding_a_page_or_log_carries_it_in() -> None:
    redactor = Redactor()
    redactor.register("password", 'p@ss w"ord')
    for text in ('p@ss w"ord', "p%40ss%20w%22ord", "p%40ss+w%22ord", '{"v": "p@ss w\\"ord"}'):
        assert redactor.reveals(text), text
        assert "ss" not in redactor.redact(text).replace("[secret:password]", ""), text


@pytest.mark.parametrize(
    ("url", "redacted"),
    [
        ("https://practice.expandtesting.com/secure", "https://practice.expandtesting.com/secure"),
        ("https://x.com/login?user=practice", "https://x.com/login?user=[secret:username]"),
        (
            "https://practice:pw@x.com:8443/practice#practice",
            "https://[secret:username]:pw@x.com:8443/[secret:username]#[secret:username]",
        ),
        ("https://practice.expandtesting.com:443/", "https://practice.expandtesting.com:443/"),
        ("https://practice.attacker.test/", "https://[secret:username].attacker.test/"),
        ("https://[secret", "https://[secret"),
    ],
)
def test_a_secret_is_redacted_from_an_address_everywhere_but_the_host_it_was_typed_on(url: str, redacted: str) -> None:
    redactor = Redactor()
    redactor.register("username", "practice", "https://practice.expandtesting.com/login")
    assert redactor.redact_url(url) == redacted
    assert redactor.redact("sign in as practice") == "sign in as [secret:username]"


def test_a_secret_running_from_the_host_into_the_path_is_still_redacted() -> None:
    redactor = Redactor()
    redactor.register("token", "example.test/abc")
    assert redactor.redact_url("https://example.test/abc") == "https://[secret:token]"


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


@pytest.mark.parametrize(
    ("origin", "allowed"),
    [
        ("https://www.example.test", True),
        ("https://accounts.eu.example.test", True),
        # One login across a site's hosts includes the bare domain: nobody writing the pattern means to exclude it.
        ("https://example.test", True),
        # The wildcard stands for whole labels, so a domain that merely ends with the same letters is another site.
        ("https://example.test.evil.test", False),
        ("https://notexample.test", False),
        # Neither the scheme nor the port is ever wildcarded.
        ("http://www.example.test", False),
        ("https://www.example.test:8443", False),
    ],
)
def test_a_secret_declared_for_a_site_covers_its_hosts_and_nothing_that_merely_looks_like_them(
    origin: str, allowed: bool
) -> None:
    ref = SecretRef(name="password", origins=("https://*.example.test",))
    assert secret_allowed(ref, origin_of(origin)) is allowed


def test_a_wildcard_that_covers_nothing_is_not_a_wildcard_that_covers_everything() -> None:
    # A pattern with no domain after it would otherwise match whatever the run opened.
    for pattern in ("https://*.", "https://*"):
        assert not secret_allowed(SecretRef(name="p", origins=(pattern,)), origin_of("https://evil.test"))


@pytest.mark.parametrize(
    ("url", "origin"),
    [
        ("https://[::1]/login", "https://[::1]"),
        ("https://[::1]:8443/", "https://[::1]:8443"),
        ("http://[2001:db8::1]:80/", "http://[2001:db8::1]"),
    ],
)
def test_an_ipv6_origin_survives_being_parsed_again(url: str, origin: str) -> None:
    # `hostname` unwraps the literal, and every check here reparses what this returns.
    assert origin_of(url) == origin
    ref = SecretRef(name="password", origins=(origin,))
    assert secret_allowed(ref, origin_of(url))
    assert not secret_allowed(ref, origin_of("https://[::2]/login"))


async def test_each_secret_keeps_the_scope_it_was_declared_with() -> None:
    held = ScopedSecrets.per_secret(
        {
            "shop": ("shop-password", ("https://*.example.test",)),
            "bank": ("bank-password", ("https://secure.bank.test",)),
            "orphan": ("nowhere", ()),
        }
    )
    assert {ref.name for ref in held.available()} == {"shop", "bank"}
    # The wildcard is not narrowed to whichever host was opened first.
    assert await held.resolve("shop", origin_of("https://accounts.example.test")) == "shop-password"
    assert await held.resolve("shop", origin_of("https://www.example.test")) == "shop-password"
    # One secret's scope is not another's.
    assert await held.resolve("bank", origin_of("https://www.example.test")) is None
    assert await held.resolve("bank", origin_of("https://secure.bank.test")) == "bank-password"
    # A secret declared for nowhere is offered nowhere, rather than everywhere.
    assert await held.resolve("orphan", origin_of("https://www.example.test")) is None
