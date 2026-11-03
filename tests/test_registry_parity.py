"""`scripts/registry_parity.py`: a pair of registries that disagree fails, and the pair that agrees passes.

The fixture is the state the 0.9.1 release left behind on 2026-10-30: PyPI on 0.9.1, `fastbrowse` and the
platform packages on npm still pointing `latest` at 0.9.0.
"""

import json
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from threading import Thread
from typing import Any

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "registry_parity.py"
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
PACKAGES = ("fastbrowse", *(f"@fastbrowse/{target}" for target in TARGETS))


def _registry(pypi: str, npm: dict[str, str], *, missing: tuple[str, ...] = ()) -> dict[str, Any]:
    """The paths a registry pair answers on, as the two registries answer them."""
    routes: dict[str, Any] = {"/pypi/fastbrowse/json": {"info": {"version": pypi}}}
    for package, version in npm.items():
        name = package.replace("/", "%2F")
        if package in missing:
            continue
        routes[f"/npm/-/package/{name}/dist-tags"] = {"latest": version}
    return routes


def _serve(routes: dict[str, Any]) -> tuple[str, HTTPServer]:
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            body = routes.get(self.path)
            if body is None:
                self.send_error(404)
                return
            payload = json.dumps(body).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *arguments: object) -> None:
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_port}", server


def _run(routes: dict[str, Any], *arguments: str) -> subprocess.CompletedProcess[str]:
    base, server = _serve(routes)
    try:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--pypi-url",
                f"{base}/pypi/fastbrowse/json",
                "--npm-url",
                f"{base}/npm",
                *arguments,
            ],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
    finally:
        server.shutdown()


def _npm_at(version: str) -> dict[str, str]:
    return {package: version for package in PACKAGES}


def test_the_release_left_the_registries_apart() -> None:
    """0.9.1 on PyPI and 0.9.0 on npm is exactly what the release run has to be stopped by."""
    routes = _registry("0.9.1", _npm_at("0.9.0"))
    result = _run(routes, "--version", "0.9.1")
    assert result.returncode == 1, result
    for package in PACKAGES:
        assert f"{package}'s npm latest is 0.9.0, not 0.9.1" in result.stderr
    assert "Publish the missing version" in result.stderr


def test_a_registry_pair_that_agrees_passes() -> None:
    result = _run(_registry("0.9.1", _npm_at("0.9.1")))
    assert result.returncode == 0, result
    assert "all name 0.9.1" in result.stdout


def test_pypi_behind_the_release_fails() -> None:
    """The other direction: npm is ahead of PyPI, which the same check has to catch."""
    result = _run(_registry("0.9.0", _npm_at("0.9.1")), "--version", "0.9.1")
    assert result.returncode == 1, result
    assert "PyPI's latest is 0.9.0, not 0.9.1" in result.stderr


def test_a_package_absent_from_npm_fails() -> None:
    """A dist-tag that cannot be read is the drift, not a reason to pass."""
    routes = _registry("0.9.1", _npm_at("0.9.1"), missing=("@fastbrowse/linux-x64",))
    result = _run(routes, "--version", "0.9.1")
    assert result.returncode == 1, result
    assert "@fastbrowse/linux-x64 could not be read on npm" in result.stderr


def test_an_unreachable_registry_fails() -> None:
    """A guard that cannot see a registry must not report agreement it did not see."""
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--pypi-url",
            "http://127.0.0.1:9/pypi/fastbrowse/json",
        ],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert result.returncode == 1, result
    assert "PyPI could not be read" in result.stderr
