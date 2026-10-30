import pytest

from fastbrowse.models import SecretRef
from fastbrowse.origins import origin_of, parse_origin
from fastbrowse.safety import ScopedSecrets, secret_allowed
from fastbrowse.safety import origin_of as secret_origin
from fastbrowse.totp import TotpError, one_time_code, totp_at


@pytest.mark.parametrize(
    ("declared", "origin", "allowed"),
    [
        ("https://*.example.com", "https://accounts.example.com", True),
        ("https://*.example.com", "https://example.com", True),
        ("https://*.example.com", "https://example.com.evil.test", False),
        ("https://*.example.com", "http://www.example.com", False),
        ("https://shop.example.com", "https://shop.example.com:443", True),
        ("https://shop.example.com", "about:blank", False),
    ],
)
def test_a_secret_is_allowed_where_the_run_would_type_it(declared: str, origin: str, allowed: bool) -> None:
    assert secret_allowed(SecretRef(name="p", origins=(declared,)), origin) is allowed


def test_origins_are_parsed_as_the_grant_and_the_secret_gate_parse_them() -> None:
    assert origin_of("https://Shop.Example.com:443/cart") == "https://shop.example.com"
    assert origin_of("about:blank") is None
    assert secret_origin("https://*.example.com/login") == "https://*.example.com"
    with pytest.raises(ValueError, match="origin"):
        parse_origin("https://*.example.com")


async def test_scoped_secrets_answer_only_on_their_own_origins() -> None:
    secrets = ScopedSecrets.per_secret({"p": ("hunter2", ("https://*.example.com",)), "none": ("x", ())})
    assert [ref.name for ref in secrets.available()] == ["p"]
    assert await secrets.resolve("p", "https://www.example.com") == "hunter2"
    assert await secrets.resolve("p", "https://evil.test") is None


async def test_a_one_time_code_is_the_rfc_6238_code() -> None:
    # RFC 6238 appendix B, SHA-1, truncated to six digits.
    assert totp_at("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59) == "287082"
    assert len(await one_time_code("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")()) == 6
    with pytest.raises(TotpError):
        one_time_code("not base32 !")
