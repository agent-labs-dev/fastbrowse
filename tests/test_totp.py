import base64

import pytest

from fastbrowse.adapters.totp import TotpError, one_time_code, totp_at

KEY = base64.b32encode(b"12345678901234567890").decode()
URI = f"otpauth://totp/example?secret={KEY}"


@pytest.mark.parametrize(("algorithm", "size", "column"), [("SHA1", 20, 0), ("SHA256", 32, 1), ("SHA512", 64, 2)])
@pytest.mark.parametrize(
    ("when", "expected"),
    [
        (59, ("94287082", "46119246", "90693936")),
        (1111111109, ("07081804", "68084774", "25091201")),
        (1111111111, ("14050471", "67062674", "99943326")),
        (1234567890, ("89005924", "91819424", "93441116")),
        (2000000000, ("69279037", "90698825", "38618901")),
        (20000000000, ("65353130", "77737706", "47863826")),
    ],
)
def test_rfc_6238_appendix_b(
    algorithm: str, size: int, column: int, when: float, expected: tuple[str, str, str]
) -> None:
    seed = (b"12345678901234567890" * 4)[:size]
    key = base64.b32encode(seed).decode()
    assert totp_at(f"otpauth://totp/example?secret={key}&algorithm={algorithm}&digits=8", when) == expected[column]


@pytest.mark.parametrize("seed", [b"12345678901234567890", b"123456789012345678901"])
def test_bare_keys_and_uris_share_defaults_and_accept_base32_variants(seed: bytes) -> None:
    key = base64.b32encode(seed).decode()
    expected = totp_at(key, 59)
    assert len(expected) == 6
    for variant in (key, key.rstrip("="), " ".join(key.lower())):
        assert totp_at(variant, 59) == expected
        assert totp_at(f"otpauth://totp/example?secret={variant}", 59) == expected


def test_defaults_zero_pad_six_digits() -> None:
    assert totp_at(KEY, 1111111109) == "081804"


@pytest.mark.parametrize(
    "key",
    [
        "",
        "   ",
        "========",
        "A",
        "INVALID!SECRET",
        "MZXW6===A",
        "\N{SNOWMAN}",
        f"otpauth://hotp/example?secret={KEY}",
        f"steam://{KEY}",
        f"https://example.com/?secret={KEY}",
        f"otpauth://[{KEY}]/example?secret={KEY}",
        "otpauth://totp/example",
        "otpauth://totp/example?secret=",
        "otpauth://totp/example?secret=INVALID!SECRET",
        f"{URI}&secret={KEY}",
        f"{URI}&algorithm=MD5",
        f"{URI}&algorithm=",
        f"{URI}&algorithm={KEY}",
        f"{URI}&digits=7",
        f"{URI}&digits=6.0",
        f"{URI}&digits=",
        f"{URI}&period=0",
        f"{URI}&period=-30",
        f"{URI}&period=1.5",
        f"{URI}&period=invalid",
        f"{URI}&period=",
        f"{URI}&period={KEY}",
    ],
)
def test_unusable_keys_fail_before_use_without_disclosing_secrets(key: str) -> None:
    for operation in (lambda: totp_at(key, 59), lambda: one_time_code(key)):
        with pytest.raises(TotpError) as raised:
            operation()
        rendered = str(raised.value) + repr(raised.value)
        for sensitive in (key, KEY, "INVALID!SECRET"):
            if sensitive.strip():
                assert sensitive not in rendered


@pytest.mark.parametrize(
    ("period", "now", "delay"),
    [(30, 20.0, 0.0), (30, 25.0, 0.0), (30, 29.5, 0.5), (8, 3.5, 0.0), (8, 4.5, 3.5), (1, 0.75, 0.25)],
)
async def test_provider_waits_only_inside_the_guard_and_reads_the_clock_again(
    period: int, now: float, delay: float
) -> None:
    key = f"{URI}&period={period}"
    slept: list[float] = []

    async def sleep(seconds: float) -> None:
        nonlocal now
        slept.append(seconds)
        # A busy event loop may resume us more than one period after the requested wake time.
        now += seconds + period

    provider = one_time_code(key, clock=lambda: now, sleep=sleep)
    assert await provider() == totp_at(KEY, (now // period) * 30)
    assert slept == ([delay] if delay else [])


@pytest.mark.parametrize("key", [KEY, URI])
async def test_provider_computes_afresh_and_its_repr_keeps_the_key_private(key: str) -> None:
    times = iter([0.0, 30.0, 60.0])

    async def sleep(seconds: float) -> None:
        raise AssertionError("a code at the start of its period must not wait")

    provider = one_time_code(key, clock=lambda: next(times), sleep=sleep)
    assert key not in repr(provider)
    assert KEY not in repr(provider)
    assert [await provider() for _ in range(3)] == ["755224", "287082", "359152"]
