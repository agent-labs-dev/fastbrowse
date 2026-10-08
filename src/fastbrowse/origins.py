"""The exact-origin grant: which documents a run may inspect and control.

It scopes documents, not the network: images, scripts and requests a granted page makes to other hosts still load.
"""

from __future__ import annotations

from collections.abc import Iterable
from urllib.parse import urlsplit

_DEFAULT_PORTS = {"http": 80, "https": 443}


def origin_of(url: str) -> str | None:
    """`scheme://host[:port]` as Chrome would compare it, or None for anything that is not an http(s) address.

    Chrome reads a backslash as a slash and drops tabs and newlines, so `https://good\\@evil/` is `evil` to Chrome
    and `good` to `urlsplit`. An address either parser could read two ways is refused rather than compared.
    """
    if any(c == "\\" or ord(c) <= 0x20 or ord(c) == 0x7F for c in url):
        return None
    try:
        parts = urlsplit(url)
        port, host = parts.port, parts.hostname
    except ValueError:
        return None
    if (
        parts.scheme not in _DEFAULT_PORTS
        or not parts.netloc
        or not host
        or not url.lower().startswith(f"{parts.scheme}://")
    ):
        return None
    try:
        host = host.encode("idna").decode("ascii")
    except UnicodeError:
        return None
    if ":" in host:
        host = f"[{host}]"
    return f"{parts.scheme}://{host}" + ("" if port in (None, _DEFAULT_PORTS[parts.scheme]) else f":{port}")


def parse_origin(value: str) -> str:
    """The normalized origin a grant entry names; raises `ValueError` for a wildcard, userinfo, path or non-http(s)."""
    parts = urlsplit(value)
    if "*" in value or "@" in parts.netloc or parts.path not in ("", "/") or "?" in value or "#" in value:
        raise ValueError("allowed_origins entries are bare http(s) origins such as https://example.com")
    origin = origin_of(value)
    if origin is None:
        raise ValueError("allowed_origins entries are bare http(s) origins such as https://example.com")
    return origin


def parse_origins(values: Iterable[str]) -> tuple[str, ...]:
    origins = tuple(dict.fromkeys(parse_origin(value) for value in values))
    if not origins:
        raise ValueError("allowed_origins is empty: pass None for an unscoped run")
    return origins


class OriginGrant:
    def __init__(self, origins: Iterable[str]) -> None:
        self.origins = frozenset(origins)

    def allows(self, url: str) -> bool:
        """Only documents at an approved exact origin are granted.

        A blank or srcdoc document can inherit content from a foreign opener, so an opaque address
        never grants access, including about:, data:, blob:, file: and javascript:.
        """
        return origin_of(url) in self.origins
