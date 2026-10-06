"""The endpoint adapter: exact-origin rewriting, cookie attributes kept, and requests forwarded to the capsule."""

import http.client
import json
import socket
import threading
import urllib.error
import urllib.request
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import ClassVar

import pytest

from fastbrowse.evals.endpoint_adapter import EndpointMapError, Rewriter, main, make_server, read_url_map

PUBLIC = "https://site.example.test"


class _Capsule(BaseHTTPRequestHandler):
    seen: ClassVar[list[dict[str, str]]] = []

    def respond(self) -> None:
        origin = f"http://{self.headers['Host']}"
        length = int(self.headers.get("Content-Length", "0") or 0)
        type(self).seen.append(
            {**dict(self.headers.items()), "_body": self.rfile.read(length).decode(), "_path": self.path}
        )
        # Written by hand: json.dumps would double the backslash of an escaped slash, which real JSON does not carry.
        escaped = origin.replace("/", "\\/")
        body = (
            f'{{"self": "{origin}", "escaped": "{escaped}", "longer": "{origin}0", "other": "http://127.0.0.2:9"}}'
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Location", origin + "/next")
        for cookie in (
            "a=1; Domain=127.0.0.1; Path=/; Secure; SameSite=None; HttpOnly",
            "b=2; domain=.LOCALHOST:99; SameSite=Strict",
            "c=3; Domain=unrelated.test; Secure",
        ):
            self.send_header("Set-Cookie", cookie)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_POST = respond

    def log_message(self, format: str, *args: object) -> None:
        pass


@pytest.fixture
def adapter() -> Iterator[tuple[str, ThreadingHTTPServer]]:
    _Capsule.seen = []
    capsule = ThreadingHTTPServer(("127.0.0.1", 0), _Capsule)
    upstream = f"http://127.0.0.1:{capsule.server_port}"
    server = make_server("s", 0, {"s": upstream}, lambda: {"s": PUBLIC})
    threads = [threading.Thread(target=item.serve_forever, daemon=True) for item in (capsule, server)]
    for thread in threads:
        thread.start()
    yield f"http://127.0.0.1:{server.server_port}", capsule
    for item in (server, capsule):
        item.shutdown()
        item.server_close()


def test_forwards_and_rewrites_only_the_exact_origin(adapter: tuple[str, ThreadingHTTPServer]) -> None:
    url, capsule = adapter
    origin = f"http://127.0.0.1:{capsule.server_port}"
    request = urllib.request.Request(
        url + "/page?x=1",
        data=b"payload",
        headers={
            "Origin": PUBLIC,
            "Referer": PUBLIC + "/a",
            "X-Forwarded-Host": "site.example.test",
            "Host": "evil.test",
        },
    )
    with urllib.request.urlopen(request) as response:
        body = json.loads(response.read())
        location = response.headers["Location"]
    assert body["self"] == PUBLIC
    assert body["escaped"] == PUBLIC  # decoded from the escaped form, which the adapter rewrote as escaped
    assert body["longer"] == origin + "0"  # a longer port is another address
    assert body["other"] == "http://127.0.0.2:9"
    assert location == PUBLIC + "/next"
    seen = _Capsule.seen[0]
    assert (seen["_path"], seen["_body"]) == ("/page?x=1", "payload")
    assert seen["Host"] == origin.removeprefix("http://")
    assert seen["Origin"] == origin
    assert seen["Referer"] == origin + "/a"
    assert seen["X-Forwarded-Host"] == origin.removeprefix("http://")


def test_lowercase_tunnel_headers_are_rewritten(adapter: tuple[str, ThreadingHTTPServer]) -> None:
    url, capsule = adapter
    connection = http.client.HTTPConnection("127.0.0.1", int(url.rsplit(":", 1)[1]))
    try:
        connection.request(
            "GET", "/", headers={"origin": PUBLIC, "referer": PUBLIC + "/a", "x-forwarded-host": "site.example.test"}
        )
        response = connection.getresponse()
        assert response.status == 200
        response.read()
    finally:
        connection.close()
    seen = {name.lower(): value for name, value in _Capsule.seen[0].items()}
    origin = f"http://127.0.0.1:{capsule.server_port}"
    assert seen["origin"] == origin and seen["referer"] == origin + "/a"
    assert seen["x-forwarded-host"] == origin.removeprefix("http://")


def test_cookie_attributes_are_kept_and_only_the_upstream_domain_changes(
    adapter: tuple[str, ThreadingHTTPServer],
) -> None:
    url, _ = adapter
    with urllib.request.urlopen(url + "/") as response:
        cookies = response.headers.get_all("Set-Cookie")
    assert cookies == [
        "a=1; Domain=site.example.test; Path=/; Secure; SameSite=None; HttpOnly",
        "b=2; domain=.site.example.test; SameSite=Strict",
        "c=3; Domain=unrelated.test; Secure",
    ]


def test_cookie_domain_ignores_the_request_host_header() -> None:
    rewriter = Rewriter("http://127.0.0.1:5", PUBLIC)
    assert rewriter.cookie("a=1; DOMAIN=127.0.0.1:5; Secure") == "a=1; DOMAIN=site.example.test; Secure"
    assert rewriter.cookie("a=1; Domain=sub.127.0.0.1; Secure") == "a=1; Domain=sub.127.0.0.1; Secure"
    assert rewriter.cookie("a=1; Path=/") == "a=1; Path=/"


def test_unreachable_capsule_is_a_502_without_detail() -> None:
    server = make_server("s", 0, {"s": "http://127.0.0.1:1"}, lambda: {"s": PUBLIC})
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with pytest.raises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/")
        assert caught.value.code == 502
    finally:
        server.shutdown()
        server.server_close()


def test_url_maps_are_validated(tmp_path: Path) -> None:
    good = tmp_path / "good.json"
    good.write_text('{"a": "http://127.0.0.1:5"}', encoding="utf-8")
    assert read_url_map(good) == {"a": "http://127.0.0.1:5"}
    for bad in (
        '{"a": "ftp://x"}',
        '{"a": "http://user:pw@x"}',
        '{"a": "http://x/path"}',
        '{"a": "http://x:notaport"}',
        '{"a": 5}',
        "[]",
        "not json",
    ):
        path = tmp_path / "bad.json"
        path.write_text(bad, encoding="utf-8")
        with pytest.raises(EndpointMapError):
            read_url_map(path)
    with pytest.raises(EndpointMapError):
        make_server("missing", 0, {"a": "http://127.0.0.1:5"}, lambda: {})


def _serve(map_path: Path) -> Iterator[tuple[str, ThreadingHTTPServer]]:
    _Capsule.seen = []
    capsule = ThreadingHTTPServer(("127.0.0.1", 0), _Capsule)
    upstream = f"http://127.0.0.1:{capsule.server_port}"
    server = make_server("s", 0, {"s": upstream}, lambda: read_url_map(map_path))
    threads = [threading.Thread(target=item.serve_forever, daemon=True) for item in (capsule, server)]
    for thread in threads:
        thread.start()
    yield f"http://127.0.0.1:{server.server_port}", capsule
    for item in (server, capsule):
        item.shutdown()
        item.server_close()


def _status(url: str, host: str | None = None) -> int:
    request = urllib.request.Request(url, headers={"Host": host} if host else {})
    try:
        with urllib.request.urlopen(request) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def test_a_public_map_corrupted_while_serving_is_a_502_before_anything_is_forwarded(tmp_path: Path) -> None:
    public = tmp_path / "public.json"
    public.write_text(json.dumps({"s": PUBLIC}), encoding="utf-8")
    for url, _ in _serve(public):
        assert _status(url + "/a") == 200
        forwarded = len(_Capsule.seen)
        for broken in ('{"s": "http://x', "[]", '{"other": "https://o.example.test"}', '{"s": ""}', '{"s": "ftp://x"}'):
            public.write_text(broken, encoding="utf-8")
            assert _status(url + "/b") == 502
            assert _status(url + "/b", host="unseen.example.test") == 502
        public.unlink()
        assert _status(url + "/c") == 502
        assert len(_Capsule.seen) == forwarded  # nothing reached the capsule while the map was bad
        public.write_text(json.dumps({"s": PUBLIC}), encoding="utf-8")
        assert _status(url + "/d") == 200


def test_a_request_for_an_unseen_host_never_sets_a_cookie_domain_from_that_host(tmp_path: Path) -> None:
    public = tmp_path / "public.json"
    public.write_text(json.dumps({"s": PUBLIC}), encoding="utf-8")
    for url, _ in _serve(public):
        request = urllib.request.Request(
            url + "/", headers={"Host": "evil.example.test", "X-Forwarded-Host": "evil.test"}
        )
        with urllib.request.urlopen(request) as response:
            cookies = response.headers.get_all("Set-Cookie")
        assert cookies is not None
        assert "evil" not in " ".join(cookies)
        assert cookies[0].startswith("a=1; Domain=site.example.test;")


def test_the_command_refuses_to_start_without_a_valid_public_origin_for_its_site(tmp_path: Path) -> None:
    upstream = tmp_path / "up.json"
    upstream.write_text('{"s": "http://127.0.0.1:5"}', encoding="utf-8")
    for body in ('{"other": "https://o.example.test"}', '{"s": ""}', "not json", '{"s": "https://h/path"}'):
        public = tmp_path / "public.json"
        public.write_text(body, encoding="utf-8")
        with pytest.raises(SystemExit) as caught:
            main(["s", "0", "--upstream-map", str(upstream), "--public-map", str(public)])
        assert caught.value.code == 2
    with pytest.raises(SystemExit):
        main(["s", "0", "--upstream-map", str(upstream)])  # a public map is required


def _raw(port: int, request_line: str) -> bytes:
    """A request written by hand, since an HTTP client would refuse to send a target that is not a path."""
    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
        connection.sendall(f"{request_line} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode())
        chunks = []
        while chunk := connection.recv(4096):
            chunks.append(chunk)
    return b"".join(chunks)


def test_a_request_target_that_is_not_a_path_never_reaches_another_host(
    adapter: tuple[str, ThreadingHTTPServer],
) -> None:
    class Trap(_Capsule):
        calls: ClassVar[int] = 0

        def respond(self) -> None:
            type(self).calls += 1
            self.send_response(200)
            self.end_headers()

        do_GET = respond

    trap = ThreadingHTTPServer(("127.0.0.1", 0), Trap)
    threading.Thread(target=trap.serve_forever, daemon=True).start()
    url, _ = adapter
    port = int(url.rsplit(":", 1)[1])
    try:
        # `upstream + "@host"` would read the capsule's address as userinfo and connect to the trap instead.
        for target in (f"@127.0.0.1:{trap.server_port}/x", f"http://127.0.0.1:{trap.server_port}/x", "*"):
            assert _raw(port, f"GET {target}").startswith(b"HTTP/1.0 400")
    finally:
        trap.shutdown()
        trap.server_close()
    assert Trap.calls == 0 and _Capsule.seen == []
    assert _raw(port, "GET //a/b?c=1").startswith(b"HTTP/1.0 200")  # a double slash is still a path


class _Latin1(_Capsule):
    def respond(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("X-Note", "caf\u00e9 " + f"http://{self.headers['Host']}/x")  # one byte, 0xE9, on the wire
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = respond


def test_a_latin1_response_header_keeps_its_bytes_through_the_rewrite() -> None:
    capsule = ThreadingHTTPServer(("127.0.0.1", 0), _Latin1)
    server = make_server("s", 0, {"s": f"http://127.0.0.1:{capsule.server_port}"}, lambda: {"s": PUBLIC})
    for item in (capsule, server):
        threading.Thread(target=item.serve_forever, daemon=True).start()
    try:
        raw = _raw(server.server_port, "GET /")
    finally:
        for item in (server, capsule):
            item.shutdown()
            item.server_close()
    assert b"X-Note: caf\xe9 " + PUBLIC.encode() + b"/x\r\n" in raw


def test_a_public_map_that_is_not_utf8_is_a_502_not_a_crash(tmp_path: Path) -> None:
    public = tmp_path / "public.json"
    public.write_text(json.dumps({"s": PUBLIC}), encoding="utf-8")
    for url, _ in _serve(public):
        assert _status(url + "/a") == 200
        public.write_bytes(b'{"s": "https://caf\xe9.example.test"}')
        assert _status(url + "/b") == 502
    with pytest.raises(EndpointMapError):
        read_url_map(public)


def test_only_the_exact_origin_is_rewritten_at_the_authority_boundary() -> None:
    rewriter = Rewriter("http://capsule.test", PUBLIC)
    kept = (
        "http://capsule.test:9999/x",  # another port on the same host
        "http://capsule.test.evil.test/",  # a longer host
        "http://capsule.test@evil.test/",  # the origin read as userinfo
        "http://capsule.test-two/",
        "http://capsule.test%2F@evil.test/",
    )
    for text in kept:
        assert rewriter.body(text.encode()) == text.encode()
        assert rewriter.header(text) == text
    for text, expected in (
        ("http://capsule.test/a?b=c#d", f"{PUBLIC}/a?b=c#d"),
        ("see (http://capsule.test).", f"see ({PUBLIC})."),
        ("http://capsule.test", PUBLIC),
        ('"http://capsule.test?next=1"', f'"{PUBLIC}?next=1"'),
        ("http://capsule.test: ok", f"{PUBLIC}: ok"),
    ):
        assert rewriter.body(text.encode()) == expected.encode()
    ported = Rewriter("http://127.0.0.1:3487", PUBLIC)
    assert ported.body(b"http://127.0.0.1:34875/ http://127.0.0.1:3487@evil.test/") == (
        b"http://127.0.0.1:34875/ http://127.0.0.1:3487@evil.test/"
    )
    assert ported.body(b"http://localhost:3487/x") == f"{PUBLIC}/x".encode()
    assert rewriter.request_header("Origin", PUBLIC + ".evil.test") == PUBLIC + ".evil.test"
