"""A public endpoint adapter for an isolated capsule: it forwards requests to the capsule's loopback address and
rewrites only that exact origin to the public one, so a hosted browser can load the site it was told to browse.

The adapter is configuration, not a site model. It reads two URL maps (`site -> origin`): the upstream map names
where the capsule listens and the public map names where a hosted browser reaches the adapter. The public map is
read on every request because a tunnel's address changes while a run is in flight, and it fails closed: a map that
cannot be read, or that no longer names the site, is a 502 before anything is forwarded, since forwarding without
rewrites would hand the browser the capsule's loopback address. Nothing outside the exact configured origin is
rewritten, cookie security attributes pass through untouched, and the cookie domain comes from the configured
public host, never from a request header a client controls.
"""

from __future__ import annotations

import argparse
import json
import re
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

TIMEOUT = 40.0
"""Seconds one forwarded request may take before the client gets a 502."""

_HOP = frozenset(
    {
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "content-encoding",
    }
)
_REQUEST_DROPPED = _HOP | {"host", "accept-encoding"}
_REWRITTEN_TYPES = ("text/", "javascript", "json", "xml")
_REWRITTEN_REQUEST_HEADERS = frozenset({"origin", "referer", "x-forwarded-host"})


class EndpointMapError(ValueError):
    """A URL map is unusable. The adapter refuses to start rather than forward to a guessed address."""


def _origin(value: object, *, where: str) -> str:
    """A bare `scheme://host[:port]`. A path, query or credentials would make the exact-origin rewrite ambiguous."""
    if not isinstance(value, str):
        raise EndpointMapError(f"{where}: expected a URL string")
    parts = urlsplit(value)
    if parts.scheme not in {"http", "https"} or not parts.hostname:
        raise EndpointMapError(f"{where}: expected an http or https origin")
    if parts.username is not None or parts.password is not None:
        raise EndpointMapError(f"{where}: an origin must not carry credentials")
    if parts.path not in {"", "/"} or parts.query or parts.fragment:
        raise EndpointMapError(f"{where}: an origin must not carry a path, query or fragment")
    try:
        parts.port  # noqa: B018 - reading the property is what raises on a malformed port
    except ValueError as exc:
        raise EndpointMapError(f"{where}: malformed port") from exc
    return f"{parts.scheme}://{parts.netloc}"


def read_url_map(path: str | Path) -> dict[str, str]:
    """A `site -> origin` map from a JSON file, each origin validated."""
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:  # ValueError covers invalid JSON and invalid UTF-8 alike
        raise EndpointMapError(f"{path}: cannot read URL map: {exc}") from exc
    if not isinstance(raw, dict):
        raise EndpointMapError(f"{path}: URL map must be a JSON object")
    return {str(site): _origin(value, where=f"{path}[{site!r}]") for site, value in raw.items()}


def public_origin(public_map: Callable[[], Mapping[str, str]], site: str) -> str:
    """The validated public origin of `site` as the map reads now. Raises `EndpointMapError` when there is none."""
    try:
        origins = public_map()
    except (EndpointMapError, OSError) as exc:
        raise EndpointMapError(f"public URL map unreadable: {exc}") from exc
    if site not in origins:
        raise EndpointMapError(f"site {site!r} is not in the public URL map")
    return _origin(origins[site], where=f"public URL map[{site!r}]")


def loopback_variants(origin: str) -> tuple[str, ...]:
    """The origin and its `localhost` spelling: a capsule that records `127.0.0.1` also emits `localhost` links."""
    variants = [origin]
    parts = urlsplit(origin)
    if parts.hostname == "127.0.0.1":
        variants.append(origin.replace("127.0.0.1", "localhost", 1))
    return tuple(variants)


def _origin_pattern(origins: Sequence[str]) -> re.Pattern[bytes]:
    # The origin must end at the URL authority's boundary. A word character, `-`, `@` or `%` continues the host or
    # port or turns what we matched into userinfo (`http://capsule:80@elsewhere`), and `.` or `:` followed by a word
    # character is a longer host or an explicit port (`http://capsule:9999`), so none of those is this origin.
    # A path, query or fragment delimiter, a quote or prose punctuation ends it and is kept as written.
    alternatives = b"|".join(re.escape(item.encode()) for item in origins)
    return re.compile(rb"(?:" + alternatives + rb")(?![\w\-@%]|[.:]\w)")


@dataclass(frozen=True)
class Rewriter:
    """Every substitution the adapter makes, bound to one upstream and one public origin."""

    upstream: str
    public: str

    @property
    def _variants(self) -> tuple[str, ...]:
        return loopback_variants(self.upstream)

    @property
    def upstream_hosts(self) -> frozenset[str]:
        return frozenset(host for item in self._variants if (host := urlsplit(item).hostname) is not None)

    @property
    def public_host(self) -> str:
        return urlsplit(self.public).hostname or ""

    def body(self, body: bytes) -> bytes:
        """Replace the exact origin, plain and as JSON-escaped (`http:\\/\\/host`), and nothing else."""
        plain = _origin_pattern(self._variants)
        escaped = _origin_pattern([item.replace("/", "\\/") for item in self._variants])
        body = plain.sub(self.public.encode(), body)
        return escaped.sub(self.public.replace("/", "\\/").encode(), body)

    def header(self, value: str) -> str:
        # Header values are latin-1 on the wire (http.client decodes them so), so a round trip through UTF-8
        # would turn one `é` byte into two characters.
        return _origin_pattern(self._variants).sub(self.public.encode(), value.encode("latin-1")).decode("latin-1")

    def request_header(self, name: str, value: str) -> str:
        """Point a browser's `Origin`, `Referer` and forwarded host back at the capsule, which only knows its own."""
        if name.lower() == "x-forwarded-host":
            return urlsplit(self.upstream).netloc if value.split(":")[0].lower() == self.public_host.lower() else value
        return _origin_pattern([self.public]).sub(self.upstream.encode(), value.encode("latin-1")).decode("latin-1")

    def cookie(self, value: str) -> str:
        """Rewrite a `Domain` attribute that names the upstream host exactly, and keep every other attribute.

        `Secure`, `SameSite`, `HttpOnly`, `Path` and expiry pass through, so a cookie is exactly as strict as the
        capsule made it. An unrelated domain is a different site's cookie and stays as it is."""
        pieces = value.split(";")
        for index, piece in enumerate(pieces[1:], start=1):
            name, separator, domain = piece.partition("=")
            if not separator or name.strip().lower() != "domain":
                continue
            leading = "." if domain.strip().startswith(".") else ""
            host = domain.strip().lstrip(".").rsplit(":", 1)[0].lower()
            if host in {item.lower() for item in self.upstream_hosts} and self.public_host:
                pieces[index] = f"{name}={leading}{self.public_host}"
        return ";".join(pieces)


class NoRedirect(HTTPRedirectHandler):
    """A redirect is the browser's to follow, so it sees the rewritten `Location` and not a hidden hop."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        return None


_OPENER = build_opener(NoRedirect)


def make_handler(site: str, upstream: str, public_map: Callable[[], Mapping[str, str]]) -> type[BaseHTTPRequestHandler]:
    """The request handler for one site. `public_map` is called per request so a changed tunnel takes effect."""

    class Handler(BaseHTTPRequestHandler):
        def forward(self) -> None:
            try:
                rewriter = Rewriter(upstream, public_origin(public_map, site))
            except EndpointMapError:
                self.send_error(502, "Public endpoint map unavailable")
                return
            length = int(self.headers.get("Content-Length", "0") or 0)
            data = self.rfile.read(length) if length else None
            headers = {k: v for k, v in self.headers.items() if k.lower() not in _REQUEST_DROPPED}
            headers["Host"] = urlsplit(upstream).netloc
            headers["Accept-Encoding"] = "identity"
            for name in headers:
                if name.lower() in _REWRITTEN_REQUEST_HEADERS:
                    headers[name] = rewriter.request_header(name, headers[name])
            if not self.path.startswith("/"):
                # `@host` or an absolute URL appended to the origin would move the authority to the client's choice.
                self.send_error(400, "Request target must be a path")
                return
            request = Request(upstream + self.path, data=data, headers=headers, method=self.command)
            try:
                response = _OPENER.open(request, timeout=TIMEOUT)
            except HTTPError as error:
                response = error
            except Exception:
                self.send_error(502, "Isolated capsule unavailable")
                return
            with response:
                body = response.read()
                kind = response.headers.get("Content-Type", "")
                if any(token in kind for token in _REWRITTEN_TYPES):
                    body = rewriter.body(body)
                self.send_response(response.status or 502)
                for name, value in response.headers.items():
                    if name.lower() in _HOP:
                        continue
                    value = rewriter.cookie(value) if name.lower() == "set-cookie" else rewriter.header(value)
                    self.send_header(name, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if self.command != "HEAD":
                    self.wfile.write(body)

        do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = forward

        def log_message(self, format: str, *args: object) -> None:
            """Silent: a request line can carry a token, and a log is a place it would be kept."""

    return Handler


def make_server(
    site: str,
    port: int,
    upstream_map: Mapping[str, str],
    public_map: Callable[[], Mapping[str, str]],
    host: str = "127.0.0.1",
) -> ThreadingHTTPServer:
    """A threaded server forwarding `site`'s requests. Raises `EndpointMapError` for a site the map lacks."""
    if site not in upstream_map:
        raise EndpointMapError(f"site {site!r} is not in the upstream URL map")
    return ThreadingHTTPServer((host, port), make_handler(site, upstream_map[site], public_map))


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Forward one capsule to a public origin, rewriting only its own.")
    parser.add_argument("site")
    parser.add_argument("port", type=int)
    parser.add_argument("--upstream-map", required=True, type=Path, help="JSON object: site -> loopback origin")
    parser.add_argument(
        "--public-map", required=True, type=Path, help="JSON object: site -> public origin, re-read per request"
    )
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args(argv)
    try:
        upstream = read_url_map(args.upstream_map)
        public_origin(lambda: read_url_map(args.public_map), args.site)
        server = make_server(args.site, args.port, upstream, lambda: read_url_map(args.public_map), args.host)
    except EndpointMapError as exc:
        parser.error(str(exc))
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
