"""A fixture server for the local evals and the audit suite that records every submission."""

import threading
from collections.abc import Generator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qsl

FIXTURES = Path(__file__).with_name("fixtures")


class Recorder:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._posts: dict[str, list[dict[str, str]]] = {}

    def add(self, path: str, fields: dict[str, str]) -> None:
        with self._lock:
            self._posts.setdefault(path, []).append(fields)

    def snapshot(self) -> dict[str, list[dict[str, str]]]:
        with self._lock:
            return {path: list(posts) for path, posts in self._posts.items()}

    def clear(self) -> None:
        with self._lock:
            self._posts.clear()


def _handler(recorder: Recorder, roots: tuple[Path, ...]) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            name = self.path.split("?", 1)[0].lstrip("/")
            for root in roots:
                target = root / name
                if target.is_file() and target.parent == root:
                    self._send(200, target.read_bytes(), attachment=None if target.suffix == ".html" else target.name)
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
def fixture_server(port: int = 0, roots: tuple[Path, ...] = (FIXTURES,)) -> Generator[tuple[str, Recorder]]:
    """Serve the pages in `roots`, the first holding a name winning, and record every POST.

    A page is served as HTML; any other file is offered as a download. `port` 0 takes a free one; the benchmark
    names its own, since the address reaches the model's prompts."""
    recorder = Recorder()
    server = ThreadingHTTPServer(("127.0.0.1", port), _handler(recorder, roots))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", recorder
    finally:
        server.shutdown()
