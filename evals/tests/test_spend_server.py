import json
import socket
from decimal import Decimal
from pathlib import Path
from threading import Event, Thread
from urllib.parse import urlsplit

import httpx
import pytest
from fastbrowse.evals.replay import Cassette
from pydantic import SecretStr

from fastbrowse_evals.spend import RequestWallet
from fastbrowse_evals.spend_server import PricedRoute, RequestBroker

from .test_request_quotes import payload, profile


def test_loopback_routes_share_wallet_and_hide_upstream_keys(tmp_path: Path) -> None:
    calls = []
    cassette = Cassette(task="offline broker contract")
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.002")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            rows = [json.loads(line) for line in (tmp_path / "wallet").read_text().splitlines()]
            assert rows[-1]["kind"] == "reserve"
            calls.append((request.url.path, request.headers["authorization"]))
            return httpx.Response(200, json={"usage": {"cost": 0.0001}})

        routes = [
            PricedRoute("/jev/v1/systemone", profile(), SecretStr("owned-jev")),
            PricedRoute(
                "/judge/chat/completions",
                profile(shape="chat_completions", endpoint="https://provider.test/v1/chat/completions"),
                SecretStr("owned-judge"),
            ),
        ]
        broker = RequestBroker(wallet, routes, cassette, transport_factory=lambda: httpx.MockTransport(upstream))
        with broker as base:
            with httpx.Client(trust_env=False) as client:
                assert client.post(base + routes[0].path, json=payload()).status_code == 200
                assert (
                    client.post(
                        base + routes[1].path,
                        json={"model": profile().model, "messages": [{"role": "user", "content": "Grade."}]},
                        headers={"authorization": "Bearer caller-key"},
                    ).status_code
                    == 200
                )
            address = urlsplit(base)
        assert wallet.committed == Decimal("0.0002")
        assert broker.admission_stops == ()
        assert calls == [("/v1/systemone", "Bearer owned-jev"), ("/v1/chat/completions", "Bearer owned-judge")]
        assert all(address.path not in exchange.path for exchange in cassette.exchanges)
        assert "owned-" not in (tmp_path / "wallet").read_text()
        with pytest.raises(OSError):
            socket.create_connection((address.hostname, address.port), timeout=1)
        with pytest.raises(RuntimeError, match="cannot be reused"):
            broker.__enter__()


def test_uninvited_and_unpriced_requests_never_dispatch(tmp_path: Path) -> None:
    def upstream(_: httpx.Request) -> httpx.Response:
        pytest.fail("unapproved request reached provider")

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        cassette = Cassette(task="offline broker contract")
        broker = RequestBroker(
            wallet,
            [PricedRoute("/jev/v1/systemone", profile(), SecretStr("owned"))],
            cassette,
            transport_factory=lambda: httpx.MockTransport(upstream),
        )
        with broker as base:
            origin = base.rsplit("/", 1)[0]
            with httpx.Client(trust_env=False) as client:
                assert client.post(origin + "/jev/v1/systemone", json=payload()).status_code == 404
                assert client.post(base + "/other", json=payload()).status_code == 422
                assert client.post(base + "/jev/v1/systemone", json=payload(model="other")).status_code == 422
                assert client.get(base + "/jev/v1/systemone").status_code == 422
        assert wallet.charges == ()
        assert len(broker.admission_stops) == 3
        assert cassette.exchanges == []


def test_http_retry_cannot_bypass_the_shared_reservation(tmp_path: Path) -> None:
    calls = []
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            return httpx.Response(503, json={"error": "unknown charge"})

        broker = RequestBroker(
            wallet,
            [PricedRoute("/jev/v1/systemone", profile(), SecretStr("owned"))],
            Cassette(task="offline broker contract"),
            transport_factory=lambda: httpx.MockTransport(upstream),
        )
        with broker as base, httpx.Client(trust_env=False) as client:
            assert client.post(base + "/jev/v1/systemone", json=payload()).status_code == 503
            response = client.post(base + "/jev/v1/systemone", json=payload())
            assert response.status_code == 402
            assert response.json()["error"]["type"] == "request_admission"
            assert "approved total" in response.json()["error"]["message"]
        assert len(calls) == 1
        assert wallet.committed == Decimal("0.001")
        assert broker.admission_stops == ("the approved total cannot hold this physical request",)


def test_exit_drains_owned_provider_calls_before_wallet_close(tmp_path: Path) -> None:
    entered, release, closing, closed = Event(), Event(), Event(), Event()
    errors = []
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:

        def upstream(_: httpx.Request) -> httpx.Response:
            entered.set()
            assert release.wait(10)
            return httpx.Response(200, json={"usage": {"cost": 0.0001}})

        broker = RequestBroker(
            wallet,
            [PricedRoute("/jev/v1/systemone", profile(), SecretStr("owned"))],
            Cassette(task="offline broker contract"),
            transport_factory=lambda: httpx.MockTransport(upstream),
        )
        base = broker.__enter__()

        def send() -> None:
            try:
                with httpx.Client(trust_env=False, timeout=10) as client:
                    assert client.post(base + "/jev/v1/systemone", json=payload()).status_code == 200
            except BaseException as error:
                errors.append(error)

        def close() -> None:
            closing.set()
            broker.__exit__()
            closed.set()

        sender = Thread(target=send)
        closer = Thread(target=close)
        sender.start()
        try:
            assert entered.wait(5)
            closer.start()
            assert closing.wait(5)
            assert not closed.wait(0.6)
        finally:
            release.set()
            sender.join(5)
            if closer.ident is None:
                closer.start()
            closer.join(5)
        assert not sender.is_alive() and not closer.is_alive()
        assert errors == []
        assert closed.is_set()
        assert wallet.committed == Decimal("0.0001")


def test_keepalive_after_exit_cannot_mutate_finished_evidence(tmp_path: Path) -> None:
    cassette = Cassette(task="offline broker contract")
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        broker = RequestBroker(wallet, [], cassette)
        with httpx.Client(trust_env=False) as client:
            with broker as base:
                assert client.post(base + "/other", json={}).status_code == 422
            exchanges, stops = list(cassette.exchanges), broker.admission_stops
            assert client.post(base + "/other", json={}).status_code == 402
            assert cassette.exchanges == exchanges
            assert broker.admission_stops == stops


@pytest.mark.parametrize(("status", "actual"), [(200, "0.002"), (307, "0.0001")])
def test_post_send_stop_keeps_receipt_and_blocks_further_calls(tmp_path: Path, status: int, actual: str) -> None:
    cassette = Cassette(task="offline broker contract")
    calls = []
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            return httpx.Response(
                status, json={"usage": {"cost": float(actual)}}, headers={"location": "https://other.test"}
            )

        broker = RequestBroker(
            wallet,
            [PricedRoute("/jev/v1/systemone", profile(), SecretStr("owned"))],
            cassette,
            transport_factory=lambda: httpx.MockTransport(upstream),
        )
        with broker as base, httpx.Client(trust_env=False, follow_redirects=True) as client:
            response = client.post(base + "/jev/v1/systemone", json=payload())
            assert response.status_code == status
            assert response.json()["usage"]["cost"] == float(actual)
            assert "location" not in response.headers
            assert client.post(base + "/jev/v1/systemone", json=payload()).status_code == 402
        assert len(calls) == len(cassette.exchanges) == 1
        assert cassette.exchanges[0].cost == float(actual)
        assert wallet.committed == Decimal(actual)
        assert len(broker.provider_stops) == 1
