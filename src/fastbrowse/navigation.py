"""Addresses supplied by the caller, never inferred from page instructions."""

import re
from ipaddress import IPv6Address
from urllib.parse import urlsplit

from fastbrowse.origins import origin_of


def task_urls(task: str, *, start: str | None = None) -> tuple[str, ...]:
    urls: dict[str, None] = {}
    task = re.sub(r"([,;])(?=https?://)", r"\1 ", task, flags=re.IGNORECASE)
    task = task.replace("](", "] (")
    for match in re.finditer(r"https?://[^\s<>\"`]+", task, re.IGNORECASE):
        url = match.group().rstrip(".,;:'\u2019\u201d\u2026")
        for marker in ("**", "__", "*", "_"):
            if task[: match.start()].endswith(marker) and url.endswith(marker):
                url = url[: -len(marker)]
                break
        for left, right in (("(", ")"), ("[", "]"), ("{", "}")):
            while url.endswith(right) and url.count(right) > url.count(left):
                url = url[:-1]
        if not _valid_address(url):
            continue
        urls[url] = None
    if start is not None and _valid_address(start):
        urls[start] = None
    return tuple(urls)


def navigation_urls(
    task: str,
    current: str,
    *,
    start: str | None = None,
    start_landing: str | None = None,
    include_start: bool = False,
) -> tuple[str, ...]:
    """Eligible destinations, excluding the current document and its initial redirect alias."""
    return tuple(
        url
        for url in task_urls(task, start=start if include_start else None)
        if not same_address(url, current)
        and not (
            start is not None
            and start_landing is not None
            and same_address(url, start)
            and same_address(start_landing, current)
        )
    )


def _valid_address(url: str) -> bool:
    try:
        parts = urlsplit(url)
        if origin_of(url) is None or not parts.hostname or parts.username is not None or parts.password is not None:
            return False
        if ":" in parts.hostname:
            IPv6Address(parts.hostname)
        elif re.fullmatch(r"[A-Za-z0-9._-]+", parts.hostname.encode("idna").decode()) is None:
            return False
        return parts.port is None or 0 < parts.port <= 65535
    except (ValueError, UnicodeError):
        return False


def same_address(left: str, right: str) -> bool:
    a, b = urlsplit(left), urlsplit(right)
    return origin_of(left) == origin_of(right) and (a.path or "/", a.query, a.fragment) == (
        b.path or "/",
        b.query,
        b.fragment,
    )
