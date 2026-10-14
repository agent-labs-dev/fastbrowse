"""Addresses supplied by the caller, never inferred from page instructions."""

import re
from ipaddress import IPv6Address
from urllib.parse import urljoin, urlsplit

from fastbrowse.origins import origin_of

# A path the task names on its own, such as "sign in at /login". It must open a word, so a fraction, a rate or
# "and/or" is not one, and its first character must not be a slash, so it never names another host. Its body is
# read like an absolute address's, so "/wiki/Example_(topic)" or "/search?tags=red,blue" is not cut short.
_TASK_PATH = re.compile(r"(?<![^\s\"'`(\[<*_])/[A-Za-z0-9_~-][^\s<>\"`]*")
_SENTENCE_END = ".,;:'\u2019\u201d\u2026"


def _trimmed(task: str, match: re.Match[str], sentence_end: str = _SENTENCE_END) -> str:
    """The address a match names, without the sentence punctuation or unmatched wrapper that closes around it."""
    url = match.group().rstrip(sentence_end)
    for marker in ("**", "__", "*", "_"):
        if task[: match.start()].endswith(marker) and url.endswith(marker):
            url = url[: -len(marker)]
            break
    for left, right in (("(", ")"), ("[", "]"), ("{", "}")):
        while url.endswith(right) and url.count(right) > url.count(left):
            url = url[:-1]
    return url


def task_urls(task: str, *, start: str | None = None, include_start: bool = True) -> tuple[str, ...]:
    urls: dict[str, None] = {}
    task = re.sub(r"([,;])(?=https?://)", r"\1 ", task, flags=re.IGNORECASE)
    task = task.replace("](", "] (")
    for match in re.finditer(r"https?://[^\s<>\"`]+", task, re.IGNORECASE):
        url = _trimmed(task, match)
        if not _valid_address(url):
            continue
        urls[url] = None
    if start is not None and _valid_address(start):
        # An entry page with nothing on it left a run told to "sign in at /login" with no address to open, so it
        # stopped stuck. The caller gave the path for the site they started the run on, so it resolves there.
        for match in _TASK_PATH.finditer(task):
            # A path asked about in a question ends the sentence with "?", which an address does not.
            url = urljoin(start, _trimmed(task, match, _SENTENCE_END + "!?"))
            if _valid_address(url):
                urls[url] = None
        if include_start:
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
        for url in task_urls(task, start=start, include_start=include_start)
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
