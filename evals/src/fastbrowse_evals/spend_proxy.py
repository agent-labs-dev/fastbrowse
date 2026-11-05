"""Physical HTTP admission for explicitly priced request routes.

Unknown charges retain their entire reservation, including transport failures
and cancellation after admission.
"""

from __future__ import annotations

from decimal import Decimal
from uuid import uuid4

import httpx
from pydantic import SecretStr, TypeAdapter

from fastbrowse_evals.request_quotes import RequestPriceProfile, json_object, quote_request
from fastbrowse_evals.spend import Money, RequestWallet, SpendStop


class ProviderResponseStop(SpendStop):
    """Stop spending while retaining a completed provider response for recording."""

    def __init__(self, reason: str, response: httpx.Response) -> None:
        super().__init__(reason)
        self.response = response


def actual_cost(body: bytes) -> Decimal | None:
    try:
        usage = json_object(body).get("usage")
        if not isinstance(usage, dict):
            return None
        cost = usage.get("cost")
        if isinstance(cost, bool) or not isinstance(cost, (Decimal, int)):
            return None
        return TypeAdapter(Money).validate_python(cost)
    except (ValueError, TypeError, ArithmeticError):
        return None


class SpendProxy(httpx.AsyncBaseTransport):
    """A caller injects this transport into each client that can incur spend.

    The default upstream has no proxy environment, redirects or transport retry.
    A supplied transport is an offline-test seam, not a second routing layer.
    """

    def __init__(
        self,
        wallet: RequestWallet,
        profile: RequestPriceProfile,
        api_key: SecretStr,
        *,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self._wallet = wallet
        self._profile = profile
        self._api_key = api_key
        self._transport = transport if transport is not None else httpx.AsyncHTTPTransport(retries=0, trust_env=False)

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        await request.aread()
        quote = quote_request(self._profile, request)
        headers = self._profile.owned_headers() | {"authorization": f"Bearer {self._api_key.get_secret_value()}"}
        # Only timeout metadata crosses this seam; forwarding other extensions could change routing.
        extensions = {"timeout": request.extensions["timeout"]} if "timeout" in request.extensions else {}
        outgoing = httpx.Request(
            "POST", self._profile.endpoint, headers=headers, content=request.content, extensions=extensions
        )
        request_id = uuid4().hex
        self._wallet.reserve(request_id, quote_id=quote.quote_id, bound=quote.reserve_usd)
        response: httpx.Response | None = None
        try:
            response = await self._transport.handle_async_request(outgoing)
            body = await response.aread()
        except BaseException:
            self._wallet.settle(request_id, None)
            raise
        finally:
            if response is not None:
                await response.aclose()
        # HTTPX already decoded the body; retaining gzip or its byte length would decode it twice.
        headers = {
            key: value
            for key, value in response.headers.items()
            if key not in {"content-encoding", "content-length", "transfer-encoding"}
        }
        receipt = httpx.Response(response.status_code, headers=headers, content=body)
        try:
            self._wallet.settle(request_id, actual_cost(body))
        except SpendStop as error:
            raise ProviderResponseStop(str(error), receipt) from error
        except OSError as error:
            raise ProviderResponseStop("provider response received but settlement journal failed", receipt) from error
        if response.is_redirect:
            raise ProviderResponseStop("priced requests do not follow provider redirects", receipt)
        return receipt

    async def aclose(self) -> None:
        await self._transport.aclose()
