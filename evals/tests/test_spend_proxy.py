import asyncio
import base64
import gzip
import json
from decimal import Decimal
from pathlib import Path

import httpx
import pytest
from pydantic import SecretStr

from fastbrowse_evals.spend import ChargeState, RequestWallet, SpendStop
from fastbrowse_evals.spend_proxy import ProviderResponseStop, SpendProxy

from .test_request_quotes import payload, profile


@pytest.mark.asyncio
async def test_only_caller_timeout_extension_reaches_upstream(tmp_path: Path) -> None:
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            assert request.extensions == {"timeout": {"connect": 1.25, "read": 1.25, "write": 1.25, "pool": 1.25}}
            return httpx.Response(200, json={})

        async with httpx.AsyncClient(
            timeout=1.25,
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream)),
        ) as client:
            await client.post(profile().endpoint, json=payload(), extensions={"unexpected-routing": "do not forward"})


@pytest.mark.asyncio
async def test_image_opt_in_header_and_original_assets_match_the_quote(tmp_path: Path) -> None:
    assets = {
        "image": {"type": "image", "format": "png", "data_base64": base64.b64encode(b"\x89PNG\r\n\x1a\n").decode()}
    }
    body = json.dumps(payload(state={"screenshot": "image"}, assets=assets)).encode()
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        assert request.headers["typesafe-danger-zone-images"] == "true"
        assert request.headers["typesafe-danger-zone-long-context"] == "true"
        assert request.content == body
        return httpx.Response(200, json={"usage": {"cost": 0.0001}})

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(
                wallet,
                profile(images=True, long_context=True),
                SecretStr("owned"),
                transport=httpx.MockTransport(upstream),
            )
        ) as client:
            assert (
                await client.post(profile().endpoint, content=body, headers={"content-type": "application/json"})
            ).status_code == 200
        assert wallet.committed == Decimal("0.0001")
        assert len(calls) == 1


@pytest.mark.asyncio
async def test_invalid_owned_key_fails_before_reserving_or_forwarding(tmp_path: Path) -> None:
    def upstream(_: httpx.Request) -> httpx.Response:
        pytest.fail("invalid credential reached upstream")

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("\N{SNOWMAN}"), transport=httpx.MockTransport(upstream))
        ) as client:
            with pytest.raises(UnicodeEncodeError):
                await client.post(profile().endpoint, json=payload())
        assert wallet.charges == ()


@pytest.mark.asyncio
async def test_compressed_receipt_is_forwarded_decoded_with_consistent_headers(tmp_path: Path) -> None:
    body = b'{"usage":{"cost":0.0001}}'
    transport = httpx.MockTransport(
        lambda _: httpx.Response(200, headers={"content-encoding": "gzip"}, content=gzip.compress(body))
    )
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=transport)
        ) as client:
            response = await client.post(profile().endpoint, json=payload())
        assert response.content == body
        assert "content-encoding" not in response.headers
        assert int(response.headers["content-length"]) == len(body)
        assert wallet.committed == Decimal("0.0001")


@pytest.mark.asyncio
async def test_reservation_is_durable_before_send_and_credentials_are_owned(tmp_path: Path) -> None:
    journal = tmp_path / "wallet.jsonl"
    with RequestWallet(journal, campaign="test", limit=Decimal("0.002")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            rows = [json.loads(row) for row in journal.read_text().splitlines()]
            assert rows[-1]["kind"] == "reserve"
            assert Decimal(rows[-1]["bound"]) == Decimal("0.001")
            assert request.headers["authorization"] == "Bearer owned"
            assert "cookie" not in request.headers
            assert "proxy-authorization" not in request.headers
            assert "x-provider-routing" not in request.headers
            assert request.headers["host"] == "provider.test"
            return httpx.Response(200, content=b'{"usage":{"cost":0.000123456789012345}}')

        proxy = SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        async with httpx.AsyncClient(transport=proxy) as client:
            response = await client.post(
                profile().endpoint,
                json=payload(),
                headers={
                    "authorization": "Bearer inherited",
                    "cookie": "secret",
                    "proxy-authorization": "other",
                    "x-provider-routing": "other",
                    "host": "other.test",
                },
            )
            assert response.status_code == 200
        assert wallet.committed == Decimal("0.000123456789012345")
        assert wallet.charges[0].state == ChargeState.SETTLED
        assert "owned" not in journal.read_text()
        assert "inherited" not in journal.read_text()


@pytest.mark.asyncio
async def test_unknown_receipts_and_retries_each_hold_a_physical_reservation(tmp_path: Path) -> None:
    calls = 0
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.002")) as wallet:

        def upstream(_: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(503, json={"error": "retry later"})

        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            for _ in range(2):
                assert (await client.post(profile().endpoint, json=payload())).status_code == 503
            with pytest.raises(SpendStop):
                await client.post(profile().endpoint, json=payload())
        assert calls == 2
        assert len({charge.request_id for charge in wallet.charges}) == 2
        assert all(charge.state == ChargeState.UNKNOWN for charge in wallet.charges)
        assert wallet.committed == Decimal("0.002")


@pytest.mark.asyncio
async def test_unpriced_request_never_reaches_upstream_or_wallet(tmp_path: Path) -> None:
    def upstream(_: httpx.Request) -> httpx.Response:
        pytest.fail("unpriced request reached upstream")

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            with pytest.raises(SpendStop):
                await client.post(profile().endpoint, json=payload(plugins=[{"id": "search"}]))
        assert wallet.charges == ()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "body",
    [
        b"not json",
        b'{"usage":{"cost":true}}',
        b'{"usage":{"cost":-1}}',
        b'{"usage":{"cost":NaN}}',
        b'{"usage":{"cost":"0.0001"}}',
        b'{"usage":{"cost":0.0000000000000000001}}',
        b'{"usage":{"cost":0,"cost":0.0001}}',
    ],
)
async def test_malformed_receipts_retain_full_unknown_charge(tmp_path: Path, body: bytes) -> None:
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        transport = httpx.MockTransport(lambda _: httpx.Response(200, content=body))
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=transport)
        ) as client:
            await client.post(profile().endpoint, json=payload())
        assert wallet.charges[0].state == ChargeState.UNKNOWN
        assert wallet.committed == Decimal("0.001")


@pytest.mark.asyncio
async def test_interrupted_response_keeps_hold_and_closes_stream(tmp_path: Path) -> None:
    class BrokenStream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            yield b'{"usage":'
            raise httpx.ReadError("interrupted")

        async def aclose(self) -> None:
            self.closed = True

    stream = BrokenStream()
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        transport = httpx.MockTransport(lambda _: httpx.Response(200, stream=stream))
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=transport)
        ) as client:
            with pytest.raises(httpx.ReadError):
                await client.post(profile().endpoint, json=payload())
        assert stream.closed
        assert wallet.charges[0].state == ChargeState.UNKNOWN


@pytest.mark.asyncio
async def test_journal_failure_never_forwards(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    def fail_write(_: object) -> None:
        raise OSError("journal unavailable")

    def upstream(_: httpx.Request) -> httpx.Response:
        pytest.fail("request reached upstream without a durable reservation")

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        monkeypatch.setattr(wallet, "_append", fail_write)
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            with pytest.raises(OSError, match="journal"):
                await client.post(profile().endpoint, json=payload())


@pytest.mark.asyncio
@pytest.mark.parametrize("error", [httpx.ConnectError("offline"), asyncio.CancelledError()])
async def test_send_failure_and_cancellation_retain_unknown_hold(tmp_path: Path, error: BaseException) -> None:
    def upstream(_: httpx.Request) -> httpx.Response:
        raise error

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            with pytest.raises(type(error)):
                await client.post(profile().endpoint, json=payload())
        assert wallet.committed == Decimal("0.001")
        assert wallet.charges[0].state == ChargeState.UNKNOWN


@pytest.mark.asyncio
async def test_redirect_is_not_followed_even_when_client_requests_it(tmp_path: Path) -> None:
    calls = 0
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:

        def upstream(_: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(307, headers={"location": profile().endpoint})

        async with httpx.AsyncClient(
            follow_redirects=True,
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream)),
        ) as client:
            with pytest.raises(SpendStop, match="redirect"):
                await client.post(profile().endpoint, json=payload())
        assert calls == 1
        assert wallet.committed == Decimal("0.001")


@pytest.mark.asyncio
async def test_charge_above_estimate_is_retained_and_blocks_next_send(tmp_path: Path) -> None:
    calls = 0
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:

        def upstream(_: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            return httpx.Response(200, json={"usage": {"cost": 0.002}})

        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            for _ in range(2):
                with pytest.raises(SpendStop):
                    await client.post(profile().endpoint, json=payload())
        assert calls == 1
        assert wallet.committed == Decimal("0.002")


@pytest.mark.asyncio
@pytest.mark.parametrize("status, cost", [(200, "0.002"), (307, "0.0001")])
async def test_post_send_stop_preserves_decoded_provider_receipt(tmp_path: Path, status: int, cost: str) -> None:
    body = ('{ "usage": { "cost": ' + cost + ' }, "id": "actual-provider-receipt" }').encode()
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        return httpx.Response(
            status,
            headers={"content-encoding": "gzip", "content-type": "application/json", "location": profile().endpoint},
            content=gzip.compress(body),
        )

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        async with httpx.AsyncClient(
            follow_redirects=True,
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream)),
        ) as client:
            with pytest.raises(ProviderResponseStop) as stopped:
                await client.post(profile().endpoint, json=payload())
        receipt = stopped.value.response
        assert receipt.status_code == status
        assert receipt.content == body
        assert receipt.headers["content-type"] == "application/json"
        assert "content-encoding" not in receipt.headers
        assert int(receipt.headers["content-length"]) == len(body)
        assert wallet.committed == Decimal(cost)
        assert len(calls) == 1


@pytest.mark.asyncio
async def test_settlement_journal_failure_preserves_completed_provider_receipt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    body = b'{"usage":{"cost":0.0001},"id":"actual-provider-receipt"}'
    calls = []

    def fail_write(_: object) -> None:
        raise OSError("journal unavailable")

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:

        def upstream(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            monkeypatch.setattr(wallet, "_append", fail_write)
            return httpx.Response(200, content=body)

        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            with pytest.raises(ProviderResponseStop) as stopped:
                await client.post(profile().endpoint, json=payload())
            with pytest.raises(SpendStop):
                await client.post(profile().endpoint, json=payload())
        assert isinstance(stopped.value.__cause__, OSError)
        assert stopped.value.response.status_code == 200
        assert stopped.value.response.content == body
        assert wallet.committed == Decimal("0.001")
        assert len(calls) == 1


@pytest.mark.asyncio
async def test_concurrent_admission_cannot_forward_more_than_remaining_hold(tmp_path: Path) -> None:
    calls = 0
    entered = asyncio.Event()
    release = asyncio.Event()
    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("0.001")) as wallet:

        async def upstream(_: httpx.Request) -> httpx.Response:
            nonlocal calls
            calls += 1
            entered.set()
            await release.wait()
            return httpx.Response(200, json={})

        async with httpx.AsyncClient(
            transport=SpendProxy(wallet, profile(), SecretStr("owned"), transport=httpx.MockTransport(upstream))
        ) as client:
            first = asyncio.create_task(client.post(profile().endpoint, json=payload()))
            await entered.wait()
            try:
                with pytest.raises(SpendStop):
                    await client.post(profile().endpoint, json=payload())
            finally:
                release.set()
                await first
        assert calls == 1


@pytest.mark.asyncio
async def test_chat_images_preserve_body_without_systemone_headers(tmp_path: Path) -> None:
    pricing = profile(shape="chat_completions", chat_images=True, image_usd_per_image="0.002")
    body = json.dumps(
        {
            "model": pricing.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Read."},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                    ],
                }
            ],
        }
    ).encode()
    calls = []

    def upstream(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        assert request.content == body
        assert not any(header.startswith("typesafe-") for header in request.headers)
        assert request.headers["authorization"] == "Bearer owned"
        return httpx.Response(200, json={"usage": {"cost": 0.0001}})

    with RequestWallet(tmp_path / "wallet", campaign="test", limit=Decimal("1")) as wallet:
        async with httpx.AsyncClient(
            transport=SpendProxy(
                wallet,
                pricing,
                SecretStr("owned"),
                transport=httpx.MockTransport(upstream),
            )
        ) as client:
            assert (
                await client.post(pricing.endpoint, content=body, headers={"content-type": "application/json"})
            ).status_code == 200
        assert len(calls) == 1
        assert wallet.charges[0].bound == Decimal("0.003")
        assert wallet.committed == Decimal("0.0001")
