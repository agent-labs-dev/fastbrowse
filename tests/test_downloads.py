"""The download capture's decisions: the headers it reads, the name it keeps, and the ceiling it reports.

No Chrome and no network: every case drives `_handle_paused`, the one entry point a real response reaches, with the
response's own headers. Each of these was a defect measured on a Slack-shaped archive download, where the response
states a size, names the file in `filename*`, and carries a signed query string in its address.
"""

from __future__ import annotations

import base64
import hashlib
from dataclasses import dataclass, field
from typing import cast

import pytest
from cdp_use.client import CDPClient

from fastbrowse.browser.session import (
    MAX_DOWNLOAD_BYTES,
    BrowserSession,
    _content_length,
    _download_filename,
    _filename_from_disposition,
    _is_download,
    _response_headers,
    _safe_filename,
)
from fastbrowse.config import Config
from fastbrowse.models import Artifact, ArtifactKind, BrowserConnection


@dataclass
class _Sink:
    artifacts: list[Artifact] = field(default_factory=list)

    async def put(self, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact:
        artifact = Artifact(
            kind=kind,
            name=name,
            mime_type=mime_type,
            size_bytes=len(content),
            sha256=hashlib.sha256(content).hexdigest(),
            uri=f"memory://{name}",
        )
        self.artifacts.append(artifact)
        return artifact


class _Fetch:
    """The two CDP commands a paused download sends, and a count of the body reads."""

    def __init__(self, body: bytes, *, base64_encoded: bool = True) -> None:
        self.body = body
        self.base64_encoded = base64_encoded
        self.body_reads = 0

    async def getResponseBody(self, params: dict[str, object], session_id: str | None = None) -> dict[str, object]:
        self.body_reads += 1
        text = base64.b64encode(self.body).decode() if self.base64_encoded else self.body.decode()
        return {"body": text, "base64Encoded": self.base64_encoded}

    async def continueRequest(self, params: dict[str, object], session_id: str | None = None) -> dict[str, object]:
        return {}


class _Send:
    def __init__(self, fetch: _Fetch) -> None:
        self.Fetch = fetch


class _Client:
    def __init__(self, fetch: _Fetch) -> None:
        self.send = _Send(fetch)


def _session(sink: _Sink, fetch: _Fetch, *, max_download_bytes: int = MAX_DOWNLOAD_BYTES) -> BrowserSession:
    session = BrowserSession(
        BrowserConnection(cdp_url="ws://127.0.0.1:9/devtools/browser/x", remote=False),
        sink,
        max_download_bytes=max_download_bytes,
    )
    session._client = cast(CDPClient, _Client(fetch))
    return session


def _event(
    *headers: tuple[str, str], url: str = "https://files.slack.com/files-pri/T1-F1/acme.zip?X-Amz-Signature=abc"
):
    return {
        "requestId": "r1",
        "request": {"url": url},
        "responseStatusCode": 200,
        "responseHeaders": [{"name": name, "value": value} for name, value in headers],
    }


ARCHIVE_HEADERS = (
    ("Content-Type", "application/zip"),
    ("Content-Disposition", "attachment; filename=\"acme.zip\"; filename*=UTF-8''acme%20slack%20export.zip"),
)


async def test_a_response_that_states_its_size_over_the_ceiling_is_refused_without_being_read() -> None:
    """The silent drop, from the other side: a multi-gigabyte export was pulled through the socket and discarded."""
    sink, fetch = _Sink(), _Fetch(b"PK" * 4096)
    session = _session(sink, fetch, max_download_bytes=1024)

    await session._handle_paused(_event(*ARCHIVE_HEADERS, ("Content-Length", "8192")), "sid")

    assert fetch.body_reads == 0, "a stated size over the ceiling is refused before the body is pulled"
    assert sink.artifacts == []
    assert session.artifacts == ()
    refused = session.downloads_refused
    assert [item.name for item in refused] == ["acme slack export.zip"]
    assert refused[0].size_bytes == 8192
    assert refused[0].limit_bytes == 1024


async def test_a_body_over_the_ceiling_with_no_stated_size_is_refused_and_reported() -> None:
    sink, fetch = _Sink(), _Fetch(b"PK" + b"x" * 4096)
    session = _session(sink, fetch, max_download_bytes=1024)

    await session._handle_paused(_event(*ARCHIVE_HEADERS), "sid")

    assert fetch.body_reads == 1
    assert sink.artifacts == []
    assert session.downloads_captured == 0
    assert [item.size_bytes for item in session.downloads_refused] == [4098]


async def test_a_fitting_download_is_kept_under_the_name_the_response_state_exactly() -> None:
    sink, fetch = _Sink(), _Fetch(b"PK\x03\x04archive")
    session = _session(sink, fetch)

    await session._handle_paused(_event(*ARCHIVE_HEADERS, ("Content-Length", "11")), "sid")

    assert [artifact.name for artifact in sink.artifacts] == ["acme slack export.zip"]
    assert session.downloads_captured == 1
    assert session.downloads_refused == ()
    assert session.artifacts == tuple(sink.artifacts)


async def test_headers_arriving_as_a_mapping_are_read_too() -> None:
    """A client that normalises `responseHeaders` into an object dropped every download without a word."""
    sink, fetch = _Sink(), _Fetch(b"PK\x03\x04archive")
    session = _session(sink, fetch)
    event = _event(url="https://files.slack.com/files-pri/T1-F1/acme.zip")
    event["responseHeaders"] = {"Content-Type": "application/zip", "Content-Disposition": "attachment; filename=x.zip"}

    await session._handle_paused(event, "sid")

    assert [artifact.name for artifact in sink.artifacts] == ["x.zip"]


async def test_a_refused_download_settles_the_navigation_that_resolved_to_it() -> None:
    """A direct link to an archive aborts its navigation; the abort is the download, kept or refused."""
    sink, fetch = _Sink(), _Fetch(b"PK" * 4096)
    session = _session(sink, fetch, max_download_bytes=1024)
    since = session.downloads_settled

    await session._handle_paused(_event(*ARCHIVE_HEADERS, ("Content-Length", "8192")), "sid")

    assert session.downloads_captured == 0
    assert session.downloads_settled == since + 1
    assert await session.download_settled(since, timeout_seconds=0.2) is True


async def test_a_refusal_is_reported_once_and_kept_as_evidence() -> None:
    sink, fetch = _Sink(), _Fetch(b"PK" * 4096)
    session = _session(sink, fetch, max_download_bytes=1024)
    await session._handle_paused(_event(*ARCHIVE_HEADERS, ("Content-Length", "8192")), "sid")

    assert len(session.pending_download_refusals()) == 1
    assert session.pending_download_refusals() == ()
    assert len(session.downloads_refused) == 1, "the refusal stays as evidence after it has been reported"


@pytest.mark.parametrize(
    ("url", "expected"),
    [
        (
            "https://files.slack.com/files-pri/T1-F1/acme_slack_export.zip?X-Amz-Signature=deadbeef",
            "acme_slack_export.zip",
        ),
        ("https://files.slack.com/files-pri/T1-F1/caf%C3%A9.zip#fragment", "café.zip"),
        ("https://files.slack.com/files-pri/T1-F1/", "download"),
    ],
)
def test_the_url_fallback_is_the_path_segment_and_never_the_signed_query(url: str, expected: str) -> None:
    assert _download_filename("", url) == expected


@pytest.mark.parametrize(
    ("disposition", "expected"),
    [
        ("attachment; filename=\"fallback.zip\"; filename*=UTF-8''caf%C3%A9%20export.zip", "café export.zip"),
        ('attachment; filename="plain.zip"', "plain.zip"),
        ('attachment; filename="has;semicolon.zip"', "has;semicolon.zip"),
        ("attachment; filename*=iso-8859-1''caf%E9.zip", "café.zip"),
        ("attachment; filename*=UTF-8'en'caf%C3%A9%20export.zip", "café export.zip"),
        ("attachment", None),
    ],
)
def test_the_disposition_name_prefers_the_one_it_states_exactly(disposition: str, expected: str | None) -> None:
    assert _filename_from_disposition(disposition) == expected


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("../../etc/passwd", "passwd"),
        ("..\\..\\windows\\system32\\config", "config"),
        ("  .hidden.  ", "hidden"),
        ("", "download"),
        ("a" * 500, "a" * 120),
    ],
)
def test_a_kept_name_is_one_safe_path_segment(name: str, expected: str) -> None:
    assert _safe_filename(name) == expected


def test_headers_are_keyed_lowercase_and_a_repeat_keeps_every_value() -> None:
    headers = _response_headers([{"name": "Content-Length", "value": "12"}, {"name": "content-length", "value": "12"}])
    assert headers["content-length"] == "12, 12"


def test_a_stated_size_is_read_only_when_it_is_one() -> None:
    assert _content_length("8192") == 8192
    assert _content_length("8192, 8192") == 8192
    assert _content_length("chunked") is None
    assert _content_length("-1") is None
    assert _content_length(None) is None


def test_a_disposition_naming_the_file_only_in_filename_star_is_still_a_download() -> None:
    assert _is_download("text/plain", "attachment; filename*=UTF-8''report.txt")


def test_the_ceiling_is_the_configs_and_a_callers_value_reaches_the_session() -> None:
    assert Config().max_download_bytes == MAX_DOWNLOAD_BYTES
    assert Config(max_download_bytes=1024).max_download_bytes == 1024
