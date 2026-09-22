"""Authenticator codes computed locally when a secret provider is awaited."""

import asyncio
import base64
import hmac
import struct
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Literal
from urllib.parse import parse_qs, urlsplit


class TotpError(ValueError):
    """The stored authenticator key cannot produce a code. The message never contains the key."""


@dataclass(frozen=True, repr=False)
class _Totp:
    secret: bytes
    algorithm: Literal["sha1", "sha256", "sha512"]
    digits: Literal[6, 8]
    period: int

    def at(self, when: float) -> str:
        counter = struct.pack(">Q", int(when // self.period))
        digest = hmac.digest(self.secret, counter, self.algorithm)
        offset = digest[-1] & 0x0F
        truncated = struct.unpack(">I", digest[offset : offset + 4])[0] & 0x7FFFFFFF
        return f"{truncated % (10**self.digits):0{self.digits}d}"


def _parse_key(key: str) -> _Totp:
    try:
        uri = urlsplit(key.strip())
    except ValueError:
        # URL parser errors can quote the input, which here contains a secret.
        raise TotpError("invalid authenticator URI") from None

    options: dict[str, str] = {}
    secret = key
    if uri.scheme:
        if uri.scheme != "otpauth" or uri.netloc.lower() != "totp":
            raise TotpError("only TOTP authenticator URIs are supported")
        query = parse_qs(uri.query, keep_blank_values=True)
        for name in ("secret", "algorithm", "digits", "period"):
            if name in query:
                if len(query[name]) != 1:
                    raise TotpError("authenticator URI has repeated parameters")
                options[name] = query[name][0]
        secret = options.get("secret", "")

    algorithm = options.get("algorithm", "SHA1").lower()
    if algorithm not in ("sha1", "sha256", "sha512"):
        raise TotpError("unsupported authenticator algorithm")
    digits: Literal[6, 8]
    match options.get("digits", "6"):
        case "6":
            digits = 6
        case "8":
            digits = 8
        case _:
            raise TotpError("authenticator digits must be 6 or 8")
    period_text = options.get("period", "30")
    # Bounded before `int`, which a vault URI with thousands of digits would make raise or overflow later.
    if not (period_text.isascii() and period_text.isdecimal() and len(period_text) <= 5) or not (
        0 < int(period_text) <= 86400
    ):
        raise TotpError("authenticator period must be a whole number of seconds up to a day")
    period = int(period_text)

    try:
        encoded = "".join(secret.split()).encode("ascii")
        decoded = base64.b32decode(encoded + b"=" * (-len(encoded) % 8), casefold=True)
    except ValueError:
        raise TotpError("authenticator secret is not valid base32") from None
    if not decoded:
        raise TotpError("authenticator secret is missing")
    return _Totp(decoded, algorithm, digits, period)


def totp_at(key: str, when: float) -> str:
    """The code `key` gives at unix time `when`. Pure; raises TotpError on an unusable key."""
    return _parse_key(key).at(when)


def one_time_code(
    key: str,
    *,
    clock: Callable[[], float] = time.time,
    sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
) -> Callable[[], Awaitable[str]]:
    """Validate `key` now and return a provider that computes a fresh code on each await.

    Wait for the next period when less than five seconds remain, capped at half the period,
    then read the clock again.
    """
    authenticator = _parse_key(key)
    guard = min(5.0, authenticator.period / 2)

    async def provide() -> str:
        now = clock()
        remaining = authenticator.period - now % authenticator.period
        if remaining < guard:
            await sleep(remaining)
            now = clock()
        return authenticator.at(now)

    return provide
