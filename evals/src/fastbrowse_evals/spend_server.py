"""Loopback admission for Rust clients using fastbrowse's existing HTTP proxy."""

from __future__ import annotations

import asyncio
import json
import secrets
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from threading import Condition
from typing import NoReturn

import httpx
from fastbrowse.evals.replay import Cassette, Forward, Proxy
from pydantic import SecretStr

from fastbrowse_evals.request_quotes import RequestPriceProfile, json_object
from fastbrowse_evals.spend import RequestWallet, SpendStop
from fastbrowse_evals.spend_proxy import ProviderResponseStop, SpendProxy


@dataclass(frozen=True)
class PricedRoute:
    path: str
    profile: RequestPriceProfile
    key: SecretStr = field(repr=False)


class _BrokerResponse(Exception):
    def __init__(self, response: tuple[int, str, str]) -> None:
        self.response = response
        super().__init__()


class _PrivateProxy(Proxy):
    def __init__(self, token: str, cassette: Cassette, forward: Forward) -> None:
        self._token = token
        self._lifecycle = Condition()
        self._accepting = True
        self._active = 0
        super().__init__(cassette, forward=forward, replay=False)

    def answer(self, path: str, raw: bytes) -> tuple[int, str, str, float]:
        prefix = f"/{self._token}/"
        if not path.startswith(prefix):
            return 404, "application/json", "{}", 0.0
        with self._lifecycle:
            if not self._accepting:
                return (
                    402,
                    "application/json",
                    json.dumps({"error": {"type": "request_admission", "message": "request broker is closed"}}),
                    0.0,
                )
            self._active += 1
        try:
            try:
                return super().answer("/" + path[len(prefix) :], raw)
            except _BrokerResponse as error:
                return (*error.response, 0.0)
        finally:
            with self._lifecycle:
                self._active -= 1
                self._lifecycle.notify_all()

    def __exit__(self, *exc: object) -> None:
        with self._lifecycle:
            self._accepting = False
        try:
            super().__exit__(*exc)
        finally:
            # A keep-alive request can outlive the listener; drain recording too.
            with self._lifecycle:
                self._lifecycle.wait_for(lambda: self._active == 0)


class RequestBroker:
    """Keep upstream keys and durable admission outside the Rust client's process.

    The unguessable URL prefix also excludes unsolicited requests from a browser
    page that can reach loopback. It is stripped before cassette recording.
    """

    def __init__(
        self,
        wallet: RequestWallet,
        routes: Sequence[PricedRoute],
        cassette: Cassette,
        *,
        transport_factory: Callable[[], httpx.AsyncBaseTransport] | None = None,
        timeout: float = 120.0,
    ) -> None:
        indexed = {(route.path, route.profile.model): route for route in routes}
        if len(indexed) != len(routes) or any(not route.path.startswith("/") for route in routes):
            raise ValueError("priced routes must have unique absolute path and model pairs")
        self._routes = indexed
        self._wallet = wallet
        self._transport_factory = transport_factory
        self._timeout = timeout
        self._condition = Condition()
        self._accepting = False
        self._entered = False
        self._stops: list[str] = []
        self._provider_stops: list[str] = []
        self._token = secrets.token_urlsafe(32)
        self._proxy = _PrivateProxy(self._token, cassette, self._forward)

    @property
    def admission_stops(self) -> tuple[str, ...]:
        with self._condition:
            return tuple(self._stops)

    @property
    def provider_stops(self) -> tuple[str, ...]:
        with self._condition:
            return tuple(self._provider_stops)

    def _stop(self, reason: str, status: int) -> NoReturn:
        with self._condition:
            self._stops.append(reason)
        raise _BrokerResponse(
            (status, "application/json", json.dumps({"error": {"type": "request_admission", "message": reason}}))
        )

    async def _send(self, route: PricedRoute, raw: bytes) -> tuple[int, str, str]:
        upstream = self._transport_factory() if self._transport_factory else None
        transport = SpendProxy(self._wallet, route.profile, route.key, transport=upstream)
        async with httpx.AsyncClient(transport=transport, timeout=self._timeout, trust_env=False) as client:
            response = await client.post(
                route.profile.endpoint, content=raw, headers={"content-type": "application/json"}
            )
            return response.status_code, response.headers.get("content-type", "application/json"), response.text

    def _forward(self, path: str, raw: bytes) -> tuple[int, str, str]:
        with self._condition:
            if not self._accepting:
                self._stop("request broker is closed", 402)
        try:
            body = json_object(raw)
            model = body.get("model")
            route = self._routes.get((path, model)) if isinstance(model, str) else None
        except (ValueError, TypeError, ArithmeticError):
            route = None
        if route is None:
            self._stop("no approved route and model for this request", 422)
        try:
            return asyncio.run(self._send(route, raw))
        except ProviderResponseStop as error:
            with self._condition:
                self._accepting = False
                self._provider_stops.append(str(error))
            response = error.response
            return response.status_code, response.headers.get("content-type", "application/json"), response.text
        except SpendStop as error:
            self._stop(str(error), 402)
        except httpx.HTTPError as error:
            raise _BrokerResponse(
                (502, "application/json", json.dumps({"error": {"type": type(error).__name__}}))
            ) from error

    def __enter__(self) -> str:
        with self._condition:
            if self._entered:
                raise RuntimeError("a request broker cannot be reused")
            self._entered = True
            self._accepting = True
        try:
            return self._proxy.__enter__() + "/" + self._token
        except BaseException:
            with self._condition:
                self._accepting = False
            raise

    def __exit__(self, *exc: object) -> None:
        with self._condition:
            self._accepting = False
        # Provider billing and recording finish before the caller closes the wallet.
        self._proxy.__exit__(*exc)
