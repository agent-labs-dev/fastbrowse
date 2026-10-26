"""A page read straight off the network, with no browser behind it.

A run that only needs to read a page pays for a browser it never uses: a Chrome that takes seconds to
start, a navigation that waits for scripts, and an index of controls nobody clicks. Browser Use's
`fetch-use` showed the saving, and most of a read is available from the HTTP response itself. So a
browserless run fetches the document, turns its markup into the same `Capture` blocks a browser capture
produces, and lets the rest of the run read and cite it unchanged.

What it cannot do is act. There are no controls in a fetched document, and pixels are not rendered, so a
step that would click or fill ends the run as blocked with the reason rather than pretending. Anything a
page builds with scripts is likewise invisible here, and a document whose readable text is a sliver of its
markup is read as exactly that: `NotFetched`, with the browser left as the only reader.
"""

import asyncio
import hashlib
import re
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from html.parser import HTMLParser
from urllib.parse import urljoin, urlsplit

import httpx

from fastbrowse.models import Artifact, Unavailable
from fastbrowse.origins import origin_of
from fastbrowse.page import (
    Action,
    ActResult,
    Block,
    BlockKind,
    BrowserError,
    Capture,
    Observation,
    Tab,
    cut_text,
)

# A page that never finishes arriving would hold a run for its whole deadline, so the read is bounded.
_TIMEOUT_SECONDS = 15.0
# Markup beyond this is not a document anyone reads as text; the browser can still be pointed at it.
_MAX_BYTES = 5 * 1024 * 1024
# Scripts, and the elements whose only content is a script or a picture, state nothing a reader can quote.
_SKIPPED = frozenset(
    {
        "script",
        "style",
        "noscript",
        "template",
        "svg",
        "canvas",
        "iframe",
        "object",
        "embed",
        "audio",
        "video",
    }
)
# Whether two runs of text belong to one block: a block-level tag ends one, an inline tag does not.
_BLOCKS = frozenset(
    {
        "address",
        "article",
        "aside",
        "blockquote",
        "dd",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "tbody",
        "tfoot",
        "thead",
        "ul",
    }
)
_HEADINGS = {f"h{level}": level for level in range(1, 7)}
# The walls a static fetch meets where a browser would pass: a challenge page, or a shell with no content
# until scripts run. Either way the fetched text is not the page and the run has to use the browser.
_WALLS = re.compile(
    r"just a moment|checking your browser|enable javascript|verification required|access denied"
    r"|are you a robot|captcha",
    re.IGNORECASE,
)
# A document whose text is under this share of its markup is a shell: a rendered page of any substance
# carries far more words than an empty template, and the browser is the only reader of what scripts build.
_MIN_TEXT_SHARE = 0.01
_HIDDEN_STYLE = re.compile(r"display\s*:\s*none|visibility\s*:\s*hidden", re.IGNORECASE)
# A page carrying this much text is not a wall telling the reader to turn scripts on.
_LITTLE_TEXT_CHARS = 480
# A redirect chain longer than this is a loop or a tracker maze, and the browser is the reader for both.
_MAX_REDIRECTS = 10
# The smallest text a fetched document needs before it is treated as the page itself rather than a stub.
_HTML_TYPES = frozenset({"text/html", "application/xhtml+xml"})


class NotFetched(Exception):
    """The document cannot be read without a browser. The message says why, and never quotes page text."""


class NeedsBrowser(Unavailable):
    """A browserless run was asked for something only a browser can do.

    Not a `BrowserError`: that one is a browser that failed, and the loop turns it into a failed step. This
    one ends the run as blocked, because nothing failed and no second attempt helps.
    """


def _hidden_classes(rules: str) -> set[str]:
    """Class names a stylesheet rule hides, such as `.gone { display: none }`.

    Not a renderer: it reads the plain class rules a page uses to keep a template, an alternate layout or a
    stale value off the screen, and nothing that depends on a cascade it cannot compute.
    """
    names: set[str] = set()
    for selector, declaration in re.findall(r"([^{}]+)\{([^{}]*)\}", rules):
        if _HIDDEN_STYLE.search(declaration):
            names.update(re.findall(r"\.([A-Za-z_-][\w-]*)", selector))
    return names


def clean(text: str) -> str:
    """Text with the whitespace a browser would collapse, as `capture.js` writes it.

    Quotes are validated against what a reader sees, so the same page has to read the same way whether it
    was captured from the DOM or fetched: a stray newline from markup would put a value on its own line and
    break a quote taken from the other reader.
    """
    text = re.sub(r"[ \t\r\f\v]+", " ", text)
    text = re.sub(r" *\n *", "\n", text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


class _Found:
    """One block of a fetched document, before its offsets are known."""

    __slots__ = ("heading_path", "href", "kind", "text")

    def __init__(self, kind: BlockKind, text: str, heading_path: tuple[str, ...] = (), href: str | None = None) -> None:
        self.kind = kind
        self.text = text
        self.heading_path = heading_path
        self.href = href


class _Reader(HTMLParser):
    """The rendered text of one document as the blocks a reader cites, without running it.

    `capture.js` gets this from the DOM a browser built, so it sees what CSS hid and what scripts added. This
    sees the served markup only. It keeps the shapes the reader relies on anyway: headings with the path that
    names what a value under them belongs to, one block per table row under its header, and an inline value
    left in the block of its label.
    """

    def __init__(self, url: str) -> None:
        super().__init__(convert_charrefs=True)
        self.blocks: list[_Found] = []
        self.title = ""
        self._url = url
        self._skip = 0
        self._buffer: list[str] = []
        self._kind = BlockKind.PARAGRAPH
        self._href: str | None = None
        self._headings: list[tuple[int, str]] = []
        self._heading: int | None = None
        self._pre = 0
        self._in_title = False
        self._tables = 0
        self._rows: list[list[str]] = []
        self._row: list[str] | None = None
        self._cell: list[str] | None = None
        self._cell_header = False
        self._header_row: int | None = None
        self._hidden: list[str] = []
        self._hidden_classes: set[str] = set()
        self._style: list[str] | None = None

    # -- the text of the document ---------------------------------------------------------------------

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = {name: value or "" for name, value in attrs}
        if tag == "style":
            # Kept rather than skipped: a rule that hides a class is the page saying a subtree is not shown.
            self._style = []
            self._skip += 1
            return
        if tag in _SKIPPED:
            self._skip += 1
            return
        if self._skip:
            return
        if tag == "title":
            self._in_title = True
            return
        if (
            "hidden" in attributes
            or _HIDDEN_STYLE.search(attributes.get("style", ""))
            or self._hidden_classes.intersection(attributes.get("class", "").split())
        ):
            # A browser never renders this subtree, so a value it holds is not one the user can see or quote.
            self._hidden.append(tag)
            self._skip += 1
            return
        if tag == "br":
            self._add("\n")
            return
        if tag in _HEADINGS:
            self._flush()
            self._heading = _HEADINGS[tag]
            self._kind = BlockKind.HEADING
            return
        if tag == "table":
            self._flush()
            self._tables += 1
            if self._tables == 1:
                self._rows, self._header_row = [], None
            return
        if self._tables:
            self._table_start(tag, attributes)
            return
        if tag == "li":
            self._flush()
            self._kind = BlockKind.LIST_ITEM
            return
        if tag == "pre":
            self._flush()
            self._pre += 1
            return
        if tag == "img" and self._cell is None and attributes.get("alt"):
            # An icon or a flag states its value only in its alt text, which no other reader would quote.
            self._add(f" [{clean(attributes['alt'])}]")
            return
        if tag == "a" and attributes.get("href") and not clean("".join(self._buffer)):
            # A link whose own text is the whole block keeps its destination, which is the one thing an
            # answer about addresses has to cite.
            self._href = urljoin(self._url, attributes["href"])
            return
        if tag in _BLOCKS:
            self._flush()

    def handle_endtag(self, tag: str) -> None:
        if tag == "style" and self._style is not None:
            self._skip = max(0, self._skip - 1)
            self._hidden_classes.update(_hidden_classes("".join(self._style)))
            self._style = None
            return
        if self._hidden and tag == self._hidden[-1]:
            self._hidden.pop()
            self._skip = max(0, self._skip - 1)
            return
        if tag in _SKIPPED:
            self._skip = max(0, self._skip - 1)
            return
        if self._skip:
            return
        if tag == "title":
            self._in_title = False
            return
        if tag in _HEADINGS:
            self._flush(closing_heading=True)
            self._heading = None
            self._kind = BlockKind.PARAGRAPH
            return
        if tag == "table" and self._tables:
            self._close_cell()
            self._tables -= 1
            if self._tables == 0:
                self._emit_table()
            return
        if self._tables:
            if tag in {"td", "th"}:
                self._close_cell()
            elif tag == "tr":
                self._close_cell()
                self._close_row()
            return
        if tag == "pre":
            self._pre = max(0, self._pre - 1)
            self._flush()
            return
        if tag == "li":
            self._flush()
            self._kind = BlockKind.PARAGRAPH
            return
        if tag == "a" and self._href is not None:
            self._flush(kind=BlockKind.LINK, href=self._href)
            self._href = None
            return
        if tag in _BLOCKS:
            self._flush()

    def handle_data(self, data: str) -> None:
        if self._style is not None:
            self._style.append(data)
            return
        if self._in_title:
            self.title += data
            return
        if self._skip:
            return
        self._add(data)

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self.handle_starttag(tag, attrs)

    def close(self) -> None:
        super().close()
        self._flush()

    # -- blocks ----------------------------------------------------------------------------------------

    def _add(self, text: str) -> None:
        if self._cell is not None:
            self._cell.append(text)
        else:
            self._buffer.append(text)

    def _table_start(self, tag: str, attributes: dict[str, str]) -> None:
        if self._tables > 1:
            return
        if tag == "tr":
            self._close_cell()
            self._row = []
            return
        if tag in {"td", "th"}:
            self._close_cell()
            self._cell, self._cell_header = [], tag == "th"
            return
        if tag == "img" and self._cell is not None and attributes.get("alt"):
            # A cell can state its value with a picture alone, the way a winner's flag does.
            self._cell.append(f" [{clean(attributes['alt'])}]")

    def _close_cell(self) -> None:
        if self._cell is None:
            return
        text = clean("".join(self._cell))
        self._cell = None
        if self._row is None:
            self._row = []
        self._row.append(text)
        if self._cell_header and self._header_row is None:
            self._header_row = len(self._rows)

    def _close_row(self) -> None:
        if self._row:
            self._rows.append(self._row)
        self._row = None

    def _emit_table(self) -> None:
        path = self._path()
        lines: list[tuple[BlockKind, str]] = []
        for index, cells in enumerate(self._rows):
            lines.append((BlockKind.TABLE, "| " + " | ".join(cell.replace("|", "\\|") for cell in cells) + " |"))
            if index == 0 and self._header_row == 0:
                # Without the separator the row above reads as prose, and a reader cannot tell a column
                # name from a value.
                lines.append((BlockKind.TABLE, "| " + " | ".join("---" for _ in cells) + " |"))
        for kind, text in lines:
            self.blocks.append(_Found(kind, text, path))
        self._rows, self._row, self._header_row = [], None, None

    def _path(self) -> tuple[str, ...]:
        return tuple(text for _level, text in self._headings)

    def _flush(self, *, kind: BlockKind | None = None, href: str | None = None, closing_heading: bool = False) -> None:
        raw = "".join(self._buffer)
        self._buffer = []
        if self._cell is not None:
            # Text of a block inside a cell belongs to that cell's row, not to a block of its own.
            self._cell.append(raw)
            return
        # A pre block keeps its line breaks, which are its content; everything else is read as prose.
        text = (raw.strip("\n").rstrip() if raw.strip() else "") if self._pre else clean(raw)
        if not text:
            return
        resolved = kind or self._kind
        self.blocks.append(_Found(resolved, text, self._path(), href))
        if closing_heading and self._heading is not None:
            # A heading names what follows it, never itself, and a lower heading ends every higher one.
            self._headings = [(level, name) for level, name in self._headings if level < self._heading]
            self._headings.append((self._heading, text))


def _blocks(html: str, url: str) -> tuple[str, list[_Found]]:
    reader = _Reader(url)
    reader.feed(html)
    reader.close()
    return clean(reader.title), reader.blocks


def _capture(url: str, status: int | None, title: str, blocks: list[_Found]) -> Capture:
    """The fetched blocks as a `Capture`, with the spans and hash a quote is validated against."""
    parts: list[str] = []
    captured: list[Block] = []
    offset = 0
    for found in blocks:
        start = offset
        parts.append(found.text)
        offset += len(found.text)
        end = offset
        parts.append("\n\n")
        offset += 2
        captured.append(
            Block(
                source_id=f"fetch/:{len(captured)}",
                kind=found.kind,
                frame_id=None,
                start=start,
                end=end,
                heading_path=found.heading_path,
                href=found.href,
            )
        )
    text = "".join(parts)
    return Capture(
        url=url,
        title=title,
        captured_at=datetime.now(UTC),
        sha256=hashlib.sha256(text.encode()).hexdigest(),
        text=text,
        blocks=tuple(captured),
    )


def _mime(response: httpx.Response) -> str:
    return response.headers.get("content-type", "").split(";")[0].strip().lower()


def eligible(url: str, allowed_origins: tuple[str, ...] | None) -> None:
    """Refuse an address that is not a document this may read, naming the reason.

    Credentials in the address are refused rather than dropped: `https://user:pass@host` would otherwise be
    fetched as an anonymous request and reported as the document that address serves.
    """
    parts = urlsplit(url)
    if parts.scheme not in {"http", "https"}:
        raise NotFetched(f"{parts.scheme or 'that'} is not an address a browserless run can fetch")
    if "@" in parts.netloc:
        raise NotFetched("an address carrying credentials is not fetched; the run would read it signed out")
    if allowed_origins is not None and origin_of(url) not in allowed_origins:
        raise NotFetched(f"{origin_of(url)} is outside the origins this run may read")


async def _read(client: httpx.AsyncClient, url: str, seconds: float) -> tuple[httpx.Response, bytes]:
    """`url`'s response and body, read as it arrives rather than after it has all landed.

    A response past the size bound is refused as soon as it passes it, so a long document, or a body that
    decodes to far more than it was sent as, cannot fill memory before the refusal is reached.
    """
    headers = {"accept": "text/html,application/xhtml+xml;q=0.9,text/plain;q=0.8"}
    # Followed here rather than by the client: a redirect to an origin outside the grant has to be refused
    # before it is sent, not after the request arrived.
    async with client.stream("GET", url, follow_redirects=False, timeout=seconds, headers=headers) as response:
        if response.is_redirect:
            return response, b""
        if response.status_code >= 400:
            raise NotFetched(f"the fetch got HTTP {response.status_code}")
        chunks: list[bytes] = []
        total = 0
        async for chunk in response.aiter_bytes():
            total += len(chunk)
            if total > _MAX_BYTES:
                raise NotFetched(f"the document is past {_MAX_BYTES} bytes, more than a text read takes")
            chunks.append(chunk)
        return response, b"".join(chunks)


async def fetch_capture(
    client: httpx.AsyncClient,
    url: str,
    *,
    allowed_origins: tuple[str, ...] | None = None,
    seconds: float = _TIMEOUT_SECONDS,
) -> Capture:
    """`url` as a `Capture`, read over HTTP, or `NotFetched` with the reason a browser is needed."""
    eligible(url, allowed_origins)
    target = url
    try:
        for _hop in range(_MAX_REDIRECTS):
            response, body = await _read(client, target, seconds)
            location = response.headers.get("location")
            if not response.is_redirect or not location:
                break
            target = urljoin(target, location)
            eligible(target, allowed_origins)
        else:
            raise NotFetched("the page redirects further than this follows")
    except httpx.HTTPError as exc:
        # A name that does not resolve is the caller's address, not an outage; either way the message is
        # all a run may report, and it never carries page text.
        raise NotFetched(f"the fetch failed: {type(exc).__name__}") from exc
    mime = _mime(response)
    final = str(response.url)
    if allowed_origins is not None and origin_of(final) not in allowed_origins:
        raise NotFetched(f"a redirect left for {origin_of(final)}, outside the origins this run may read")
    try:
        text = body.decode(response.encoding or "utf-8", errors="replace")
    except LookupError as exc:
        raise NotFetched("the document's character set is not one this can decode") from exc
    if mime == "text/plain":
        if not text.strip():
            raise NotFetched("the document is empty")
        return _capture(final, response.status_code, "", [_Found(BlockKind.PARAGRAPH, clean(text))])
    if mime not in _HTML_TYPES:
        raise NotFetched(f"{mime or 'the response'} is not a document read as text")
    title, blocks = _blocks(text, final)
    readable = sum(len(found.text) for found in blocks)
    if _WALLS.search(text[:20_000]) and readable < _LITTLE_TEXT_CHARS:
        raise NotFetched("the page is a bot check or an empty shell that scripts fill")
    if not readable or readable < int(len(text) * _MIN_TEXT_SHARE):
        # A page whose markup is scripts and styles has nothing to quote: the browser is the only reader of
        # what they build, and reading the shell as the page would answer from nothing. A genuinely short
        # document passes, because its text is most of its markup however few characters that is.
        raise NotFetched("the page renders its content with scripts, so the markup holds no readable text")
    return _capture(final, response.status_code, title, blocks)


class FetchedPage:
    """A `Page` served by HTTP: it reads documents and cannot touch them.

    Everything above the page seam reads `Capture` blocks and cites their spans, so a fetched document is
    read and cited exactly as a captured one. The methods that would move a page or inspect pixels raise
    `NeedsBrowser`, which ends the run as blocked instead of failing a step or inventing an answer.
    """

    def __init__(
        self,
        client: httpx.AsyncClient,
        *,
        allowed_origins: tuple[str, ...] | None = None,
        viewport_text_chars: int = 6000,
        seconds: float = _TIMEOUT_SECONDS,
        check_access: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._client = client
        self._origins = allowed_origins
        self._viewport_chars = viewport_text_chars
        self._timeout = seconds
        self._check_access = check_access
        self._capture: Capture | None = None
        self._status: int | None = None
        self._generation = 0

    @property
    def artifacts(self) -> tuple[Artifact, ...]:
        """A fetched run downloads nothing, so it has nothing to report."""
        return ()

    async def navigate(self, url: str, *, back_to: str | None = None) -> None:
        await self._gate()
        try:
            capture = await fetch_capture(self._client, url, allowed_origins=self._origins, seconds=self._timeout)
        except NotFetched as exc:
            if back_to is not None:
                # This was a guessed address, not the caller's page: the loop falls back to `start` on a
                # `BrowserError`, and concluding that the task needs a browser here would end the run.
                raise BrowserError(str(exc)) from exc
            raise NeedsBrowser(str(exc)) from exc
        self._capture = capture
        self._status = None
        self._generation += 1

    async def _gate(self) -> None:
        """The caller's gate, asked again: permission can be withdrawn while the run is reading."""
        if self._check_access is not None:
            await self._check_access()

    async def observe(self) -> Observation:
        await self._gate()
        capture = self._loaded()
        return Observation(
            url=capture.url,
            title=capture.title,
            page_key=capture.sha256,
            captured_at=capture.captured_at,
            response_status=self._status,
            document_key=capture.sha256,
            controls=(),
            omitted_controls=0,
            viewport_text=cut_text(capture.text, self._viewport_chars),
            # Nothing here can be scrolled, clicked or looked at, so no step may be chosen to do it.
            tabs=(Tab(id="fetched", url=capture.url, title=capture.title, active=True, opener_id=None),),
            history_entry=capture.url,
        )

    async def document_changed(self, observation: Observation) -> bool:
        return self._capture is None or self._capture.sha256 != observation.page_key

    async def capture(self) -> Capture:
        await self._gate()
        return self._loaded()

    async def act(self, action: Action, observation: Observation) -> ActResult:
        await self._gate()
        raise NeedsBrowser(
            "this run reads pages over HTTP and has no controls to act on; "
            "run it again without fetch mode to use a browser"
        )

    async def redrawn(self, observation: Observation, timeout_seconds: float, *, target_id: str | None = None) -> bool:
        """Nothing redraws a fetched document, so nothing it offered can have changed."""
        return False

    async def screenshot(self) -> bytes:
        await self._gate()
        raise NeedsBrowser("a browserless run renders no pixels, so it has no screenshot to take")

    def withhold_frames(self, withheld: bool) -> None:
        """There are no frames to hold back."""

    def lifecycle_mark(self) -> int:
        return self._generation

    async def wait_for_lifecycle(self, mark: int, seconds: float) -> bool:
        """A fetched document begins and ends with its response, so no later transition can arrive."""
        await asyncio.sleep(0)
        return False

    async def address(self) -> str:
        await self._gate()
        return self._loaded().url

    async def response_status(self) -> int | None:
        return self._status

    def _loaded(self) -> Capture:
        if self._capture is None:
            raise NeedsBrowser("no page has been fetched yet")
        return self._capture


__all__ = ["FetchedPage", "NeedsBrowser", "NotFetched", "eligible", "fetch_capture"]
