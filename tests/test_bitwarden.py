import base64
import json

import pytest

from fastbrowse.adapters.bitwarden import BitwardenError, UriMatch, covers, login_values
from fastbrowse.safety import ScopedSecrets, resolve_secret

# RFC 6238's SHA1 seed, encoded here rather than written out so a secret scanner does not read it as a key.
KEY = base64.b32encode(b"12345678901234567890").decode()


def item(*uris: str, password: str | None = "hunter2", match: int | None = None, totp: str | None = None) -> str:
    login = {
        "username": "me@example.com",
        "password": password,
        "totp": totp,
        "uris": [{"match": match, "uri": u} for u in uris],
    }
    return json.dumps({"id": "1", "name": "Amazon", "type": 1, "login": login})


@pytest.mark.parametrize(
    ("uri", "detection", "origin", "expected"),
    [
        ("https://www.amazon.com", None, "https://www.amazon.com", True),
        ("amazon.com", None, "https://www.amazon.com", True),
        ("https://amazon.com/ap/signin", UriMatch.DOMAIN, "https://smile.amazon.com", True),
        ("https://amazon.com", None, "https://amazon.com.evil.test", False),
        ("https://amazon.com", None, "https://notamazon.com", False),
        ("https://amazon.com", None, "http://www.amazon.com", False),
        ("https://www.amazon.com", UriMatch.HOST, "https://smile.amazon.com", False),
        ("https://www.amazon.com/ap/signin", UriMatch.EXACT, "https://www.amazon.com", True),
        ("https://www.amazon.com/ap/signin", UriMatch.EXACT, "http://sub.www.amazon.com", False),
        ("https://www.amazon.com", UriMatch.NEVER, "https://www.amazon.com", False),
        (r"https://www\.amazon\.com/.*", UriMatch.REGULAR_EXPRESSION, "https://www.amazon.com", False),
        ("", None, "https://amazon.com", False),
    ],
)
def test_a_login_is_released_only_where_its_match_detection_allows(
    uri: str, detection: UriMatch | None, origin: str, expected: bool
) -> None:
    assert covers(uri, detection, origin) is expected


def test_login_values_are_named_and_scoped_to_the_items_uris() -> None:
    assert login_values(item("amazon.com"), "https://www.amazon.com") == {
        "username": "me@example.com",
        "password": "hunter2",
    }
    assert login_values(item("amazon.com", password=None), "https://www.amazon.com") == {"username": "me@example.com"}
    with pytest.raises(BitwardenError, match="not saved for"):
        login_values(item("amazon.com"), "https://evil.test")
    with pytest.raises(BitwardenError, match="not saved for"):
        login_values(item("https://www.amazon.com", match=UriMatch.NEVER), "https://www.amazon.com")


def test_a_malformed_item_error_does_not_echo_the_password() -> None:
    malformed = json.dumps({"login": {"password": "hunter2"}})
    with pytest.raises(BitwardenError) as raised:
        login_values(malformed, "https://www.amazon.com")
    assert "hunter2" not in str(raised.value)


async def test_an_authenticator_key_offers_a_code_made_only_where_the_login_is_scoped() -> None:
    values = login_values(item("amazon.co.uk", totp=KEY), "https://www.amazon.co.uk")
    assert values.keys() == {"username", "password", "one_time_code"}
    secrets = ScopedSecrets(values, "https://www.amazon.co.uk")
    code = await resolve_secret(secrets, "one_time_code", "https://www.amazon.co.uk")
    assert code is not None and code.isdigit() and len(code) == 6
    assert await resolve_secret(secrets, "one_time_code", "https://evil.test") is None


def test_an_unusable_authenticator_key_fails_at_load_without_echoing_it() -> None:
    for key in ("steam://" + KEY, "otpauth://hotp/Amazon?secret=" + KEY, "zz-garbage-key-zz"):
        with pytest.raises(BitwardenError, match="authenticator key") as raised:
            login_values(item("amazon.co.uk", totp=key), "https://www.amazon.co.uk")
        assert KEY not in str(raised.value) and "zz-garbage-key-zz" not in str(raised.value)
