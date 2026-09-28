"""A fixture server for the audit suite.

It serves the repository's own eval fixtures unchanged, plus a small audit-only page that offers a file to
download, and it records every POST it receives so a case can check what a run actually submitted.

The eval fixtures are read, never changed: the audit suite layers on top of them.
"""

import threading
from collections.abc import Callable, Generator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qsl

from fastbrowse.adapters.local_chrome import free_port
from fastbrowse.evals.local import FIXTURES as EVAL_FIXTURES
from fastbrowse.evals.local import Recorder

AUDIT_FIXTURES = Path(__file__).with_name("fixtures")

DOWNLOAD_NAME = "audit-report.csv"
DOWNLOAD_BODY = b"city,population\nLyon,522250\n"


def _handler(recorder: Recorder) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            path = self.path.split("?", 1)[0]
            if path == f"/{DOWNLOAD_NAME}":
                self._send(200, DOWNLOAD_BODY, attachment=DOWNLOAD_NAME)
                return
            for directory in (AUDIT_FIXTURES, EVAL_FIXTURES):
                target = directory / path.lstrip("/")
                if target.is_file() and target.parent == directory:
                    self._send(200, target.read_bytes())
                    return
            self.send_error(404)

        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length") or 0)
            recorder.add(self.path, dict(parse_qsl(self.rfile.read(length).decode())))
            self._send(200, b"<!doctype html><title>Thanks</title><h1>Thanks, we received it.</h1>")

        def _send(self, status: int, body: bytes, *, attachment: str | None = None) -> None:
            self.send_response(status)
            self.send_header("Content-Type", "application/octet-stream" if attachment else "text/html; charset=utf-8")
            if attachment:
                self.send_header("Content-Disposition", f'attachment; filename="{attachment}"')
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


@contextmanager
def fixture_server() -> Generator[tuple[str, Callable[[], dict[str, list[dict[str, str]]]]]]:
    """Yield the fixture server's base URL and a reader for its POST record."""
    recorder = Recorder()
    server = ThreadingHTTPServer(("127.0.0.1", free_port()), _handler(recorder))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", recorder.snapshot
    finally:
        server.shutdown()
