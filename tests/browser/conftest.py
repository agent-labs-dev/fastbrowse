"""Fixtures for browser tests of the Rust core through the package: a real headless Chrome plus two-host fixture sites.

Skips browser fixtures when Chrome is not installed, since these tests exercise
real CDP mechanics (OOPIFs, downloads, dialogs) that cannot be faked without losing their signal.
"""

from __future__ import annotations

import hashlib
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fastbrowse.evals.harness import chrome, find_chrome
from fastbrowse.models import Artifact, ArtifactKind

SITES = Path(__file__).parent / "sites"


class RecordingArtifactSink:
    """Test double for `ArtifactSink`: keeps every artifact in memory for assertions."""

    def __init__(self) -> None:
        self.artifacts: list[Artifact] = []

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


def _handler_for(
    directory: Path, iframe_origin: str | None = None, posted: list[str] | None = None
) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_POST(self) -> None:
            # What a page sent is the one view of it a test has when the browser belongs to another process.
            body = self.rfile.read(int(self.headers["Content-Length"])).decode()
            if posted is not None:
                posted.append(body)
            self.send_response(204)
            self.end_headers()

        def do_GET(self) -> None:
            if self.path.startswith("/http-error/"):
                status = int(self.path.rsplit("/", 1)[1])
                body = b"<html><body>Sorry</body></html>"
                self.send_response(status)
                self.send_header("Content-Type", "text/html")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if self.path.startswith("/report.csv") or self.path == "/export-report":
                body = b"name,total\nReport,139.79\n"
                if self.path not in {"/report.csv?short", "/report.csv?partial"}:
                    body += b"item,1\n" * 10000
                self.send_response(206 if self.path == "/report.csv?partial" else 200)
                if self.path == "/report.csv?partial":
                    self.send_header("Content-Range", f"bytes 0-{len(body) - 1}/1000")
                self.send_header(
                    "Content-Type", "text/plain" if self.path == "/export-report" else "text/csv; charset=utf-8"
                )
                if self.path == "/export-report":
                    self.send_header("Content-Disposition", 'attachment; filename="report.CSV"')
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if self.path.startswith("/files/"):
                body = b"PK fixture release archive"
                self.send_response(200)
                self.send_header("Content-Type", "application/zip")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if self.path == "/download":
                body = b"fastbrowse fixture attachment bytes for download checksum test"
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Disposition", 'attachment; filename="spike.bin"')
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            path = self.path.split("?", 1)[0]
            if path in ("/", "/index.html") and iframe_origin is not None:
                content = (
                    (directory / "index.html").read_text(encoding="utf-8").replace("__IFRAME_ORIGIN__", iframe_origin)
                )
                body = content.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            target = directory / path.lstrip("/")
            if not target.is_file():
                self.send_response(404)
                self.end_headers()
                return
            body = target.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


@pytest.fixture(scope="session")
def iframe_site() -> Iterator[str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(SITES / "iframe"))
    port = server.server_address[1]
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        yield f"http://localhost:{port}"
    finally:
        server.shutdown()


@pytest.fixture(scope="session")
def main_site(iframe_site: str) -> Iterator[str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(SITES / "main", iframe_origin=iframe_site))
    port = server.server_address[1]
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        yield f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()


@pytest.fixture(scope="session")
def cdp_url() -> Iterator[str]:
    """The DevTools websocket of one headless Chrome the session shares."""
    if find_chrome() is None:
        pytest.skip("Chrome is not installed")
    with chrome() as url:
        yield url
