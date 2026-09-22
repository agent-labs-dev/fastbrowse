"""Login values from a Bitwarden vault item, read through the `bw` CLI the user has unlocked.

The item's own saved URIs decide where it may be used, under each URI's match detection
(https://bitwarden.com/help/uri-match-detection/). Secrets here are scoped to an origin, not a page, so
Starts with and Exact narrow to the URI's origin; a regular expression is not trusted, and a URI set to Never
matches nothing. A login saved for https is not released over http.
"""

import subprocess
from enum import IntEnum
from typing import assert_never
from urllib.parse import urlsplit

from pydantic import BaseModel, ConfigDict, ValidationError

from fastbrowse.adapters.totp import TotpError, one_time_code
from fastbrowse.models import SecretValue


class BitwardenError(RuntimeError):
    """The vault is locked, the item is missing, or it does not belong to the start origin."""


class UriMatch(IntEnum):
    """Bitwarden's stored codes; an unset match means the account default, base domain."""

    DOMAIN = 0
    HOST = 1
    STARTS_WITH = 2
    EXACT = 3
    REGULAR_EXPRESSION = 4
    NEVER = 5


class _Uri(BaseModel):
    model_config = ConfigDict(extra="ignore")
    uri: str | None = None
    match: UriMatch | None = None


class _Login(BaseModel):
    model_config = ConfigDict(extra="ignore")
    username: str | None = None
    password: str | None = None
    totp: str | None = None
    uris: list[_Uri] | None = None


class _Item(BaseModel):
    model_config = ConfigDict(extra="ignore")
    name: str
    login: _Login | None = None


def covers(uri: str, detection: UriMatch | None, origin: str) -> bool:
    """True when Bitwarden would fill the login saved for `uri` on `origin`."""
    # Vault URIs are often saved bare ("amazon.com"), which urlsplit reads as a path, not a host.
    saved = urlsplit(uri if "://" in uri else f"https://{uri}")
    visited = urlsplit(origin)
    if not saved.hostname or visited.scheme != saved.scheme:
        return False
    match detection or UriMatch.DOMAIN:
        case UriMatch.DOMAIN:
            base = saved.hostname.removeprefix("www.")
            host = visited.hostname or ""
            return host == base or host.endswith(f".{base}")
        case UriMatch.HOST | UriMatch.STARTS_WITH | UriMatch.EXACT:
            return visited.netloc.lower() == saved.netloc.lower()
        case UriMatch.REGULAR_EXPRESSION | UriMatch.NEVER:
            return False
        case _:
            assert_never(detection)


def login_values(item_json: str, origin: str) -> dict[str, SecretValue]:
    """The item's username, password and authenticator code, keyed by the secret names the agent sees."""
    try:
        item = _Item.model_validate_json(item_json)
    except ValidationError:
        # Pydantic's message quotes the input, which here holds the password.
        raise BitwardenError("bw returned something other than a vault item") from None
    if item.login is None:
        raise BitwardenError(f"Bitwarden item {item.name!r} is not a login")
    if not any(u.uri and covers(u.uri, u.match, origin) for u in item.login.uris or ()):
        raise BitwardenError(f"Bitwarden item {item.name!r} is not saved for {origin}")
    values: dict[str, SecretValue] = {
        name: value for name, value in (("username", item.login.username), ("password", item.login.password)) if value
    }
    if item.login.totp:
        # Amazon's two-step sign-in stopped a run at "Enter OTP" with the key in the vault. The code is made
        # when it is typed, and a key that cannot make one fails here rather than at that prompt.
        try:
            values["one_time_code"] = one_time_code(item.login.totp)
        except TotpError as exc:
            raise BitwardenError(
                f"Bitwarden item {item.name!r} has an authenticator key fastbrowse cannot use: {exc}"
            ) from None
    return values


def bitwarden_login(item: str, origin: str) -> dict[str, SecretValue]:
    """Read `item` (a name or id) from the unlocked vault; `BW_SESSION` must be in the environment."""
    try:
        done = subprocess.run(
            ["bw", "get", "item", item, "--nointeraction"], capture_output=True, text=True, timeout=30, check=False
        )
    except FileNotFoundError:
        raise BitwardenError("the Bitwarden CLI (bw) is not installed") from None
    if done.returncode != 0:
        # bw reports a locked vault and a missing item on stderr; neither carries a secret. An expired CLI
        # login surfaces as "Not found" above a Node stack trace, which hides the one fix that works.
        detail = done.stderr.strip()
        if "invalid_grant" in detail:
            raise BitwardenError("the bw CLI's login has expired: run bw login, then bw unlock")
        raise BitwardenError(f"bw get item failed: {detail.splitlines()[0] if detail else 'no output'}")
    return login_values(done.stdout, origin)
