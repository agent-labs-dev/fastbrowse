"""One physical-request wallet for a local fastbrowse campaign and its judges."""

from __future__ import annotations

import hashlib
import json
import os
import time
from collections.abc import Callable, Mapping
from contextlib import ExitStack
from datetime import UTC, datetime
from decimal import Decimal
from importlib.metadata import distribution
from pathlib import Path

import httpx
from fastbrowse.evals.replay import Cassette, Exchange, request_key
from parallax.models import Model
from pydantic import JsonValue, SecretStr, TypeAdapter, model_validator

from fastbrowse_evals.judging import JUDGE_MODEL
from fastbrowse_evals.request_quotes import RequestPriceProfile, json_object
from fastbrowse_evals.spend import POLICY, Money, RequestWallet, SpendStop
from fastbrowse_evals.spend_proxy import ProviderResponseStop, SpendProxy, actual_cost
from fastbrowse_evals.spend_server import PricedRoute, RequestBroker

OPENROUTER = "https://openrouter.ai/api/v1"


def runtime_identity() -> dict[str, object]:
    installed = distribution("fastbrowse")
    files = {
        str(path): hashlib.sha256(installed.locate_file(path).read_bytes()).hexdigest()
        for path in installed.files or ()
        if path.parts[0] == "fastbrowse" and path.suffix in {".py", ".so", ".pyd"}
    }
    if not any(Path(name).suffix in {".so", ".pyd"} for name in files):
        raise SpendStop("request mode requires an identifiable installed fastbrowse wheel")
    return {"fastbrowse_version": installed.version, "files_sha256": dict(sorted(files.items()))}


class RequestSessionSpec(Model):
    decision: RequestPriceProfile
    text: RequestPriceProfile
    judge: RequestPriceProfile

    @model_validator(mode="after")
    def canonical_routes(self) -> RequestSessionSpec:
        if self.decision.shape != "systemone" or self.decision.endpoint != OPENROUTER + "/systemone":
            raise ValueError("the initial request protocol requires OpenRouter System One")
        if any(
            p.shape != "chat_completions" or p.endpoint != OPENROUTER + "/chat/completions"
            for p in (self.text, self.judge)
        ):
            raise ValueError("the initial request protocol requires OpenRouter chat completions")
        if self.judge.model != JUDGE_MODEL:
            raise ValueError("the request protocol preserves the pinned browser judge")
        return self

    def provenance(self) -> dict[str, object]:
        return {
            "budget_policy": POLICY,
            "request_profile_sha256": hashlib.sha256(
                json.dumps(self.model_dump(mode="json"), sort_keys=True).encode()
            ).hexdigest(),
            "request_profiles": self.model_dump(mode="json"),
        }


class _JudgeUpstream(httpx.AsyncBaseTransport):
    def __init__(self, transport: httpx.AsyncBaseTransport) -> None:
        self.transport = transport
        self.sent = 0

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.sent += 1
        return await self.transport.handle_async_request(request)

    async def aclose(self) -> None:
        await self.transport.aclose()


class _JudgeTransport(httpx.AsyncBaseTransport):
    def __init__(self, session: RequestSession, proxy: SpendProxy, upstream: _JudgeUpstream) -> None:
        self.session, self.proxy, self.upstream = session, proxy, upstream
        self.costs: list[float | None] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.session.check_admission()
        began = time.monotonic()
        try:
            response = await self.proxy.handle_async_request(request)
        except ProviderResponseStop as exc:
            self.session.stopped = str(exc)
            # A settled response is evidence even when it stops later spending.
            response = exc.response
        except SpendStop as exc:
            self.session.stopped = str(exc)
            raise
        amount = actual_cost(response.content)
        cost = None if amount is None else float(amount)
        self.costs.append(cost)
        body = json.loads(request.content)
        path = "/judge/chat/completions"
        self.session.judge_cassette.exchanges.append(
            Exchange(
                key=request_key(path, body),
                path=path,
                purpose="judge",
                model=self.session.spec.judge.model,
                request=body,
                status=response.status_code,
                content_type=response.headers.get("content-type", "application/json"),
                body=response.text,
                latency=time.monotonic() - began,
                cost=cost,
            )
        )
        return response

    async def aclose(self) -> None:
        await self.proxy.aclose()


class RequestJudgeClient(httpx.AsyncClient):
    def __init__(self, transport: _JudgeTransport) -> None:
        self.receipts = transport
        super().__init__(transport=transport, timeout=120, trust_env=False, follow_redirects=False)

    @property
    def partial_spend(self) -> tuple[float, int]:
        costs = self.receipts.costs
        return (
            sum(cost for cost in costs if cost is not None),
            self.receipts.upstream.sent - len(costs) + sum(cost is None for cost in costs),
        )


class RequestSession:
    def __init__(
        self,
        spec: RequestSessionSpec,
        journal: Path,
        *,
        limit: Decimal,
        campaign: str,
        credentials: Mapping[str, str],
        transport_factory: Callable[[], httpx.AsyncBaseTransport] | None = None,
    ) -> None:
        self.spec, self.journal, self.limit, self.campaign = spec, journal, limit, campaign
        self.runtime = runtime_identity()
        artifacts = (
            journal,
            *(
                journal.with_suffix(suffix)
                for suffix in (".agent.jsonl.gz", ".judge.jsonl.gz", ".credit.json", ".evidence.json")
            ),
        )
        if any(path.exists() for path in artifacts):
            raise SpendStop("a request session requires fresh wallet and evidence paths")
        self._keys = {
            profile.credential_id: SecretStr(credentials[profile.credential_id])
            for profile in (spec.decision, spec.text, spec.judge)
        }
        if any(not key.get_secret_value() for key in self._keys.values()):
            raise ValueError("every approved route requires its owned credential")
        self._transport_factory = transport_factory
        self._stack = ExitStack()
        self._entered = False
        self._active = False
        self.stopped: str | None = None
        self.cassette = Cassette(task="physical request campaign")
        self.judge_cassette = Cassette(task="physical request campaign judges")
        self.wallet: RequestWallet
        self.broker: RequestBroker
        self._base: str

    async def verify_credit(self) -> None:
        checked = []
        for identity, key in self._keys.items():
            upstream = self._transport_factory() if self._transport_factory else None
            async with httpx.AsyncClient(
                transport=upstream, trust_env=False, follow_redirects=False, timeout=30
            ) as client:
                headers = {"authorization": "Bearer " + key.get_secret_value()}
                response = await client.get(OPENROUTER + "/key", headers=headers)
                response.raise_for_status()
                body = json_object(response.content)
                data = body.get("data")
                if not isinstance(data, dict) or "limit_remaining" not in data:
                    raise SpendStop("the provider returned no usable key-credit evidence")
                remaining = data["limit_remaining"]
                response = await client.get(OPENROUTER + "/credits", headers=headers)
                response.raise_for_status()
                credits = json_object(response.content).get("data")
                if not isinstance(credits, dict):
                    raise SpendStop("the provider returned no usable account-credit evidence")
                total = TypeAdapter(Money).validate_python(credits.get("total_credits"))
                used = TypeAdapter(Money).validate_python(credits.get("total_usage"))
                available = TypeAdapter(Money).validate_python(total - used)
                if remaining is not None:
                    available = min(available, TypeAdapter(Money).validate_python(remaining))
                effective = data.get("effective_budget")
                budgets = data.get("budgets", [])
                if not isinstance(budgets, list):
                    raise SpendStop("the provider returned invalid budget evidence")
                for budget in ([effective] if effective is not None else []) + budgets:
                    if not isinstance(budget, dict) or "remaining_usd" not in budget:
                        raise SpendStop("the provider returned invalid budget evidence")
                    available = min(available, TypeAdapter(Money).validate_python(budget["remaining_usd"]))
                # Recorded, never a refusal: the key's own cap is the spend control, so a campaign starts
                # whatever the provider says is left and the wallet bounds what it may actually commit.
                checked.append({"credential_id": identity, "available_usd": str(available)})
        self.journal.parent.mkdir(parents=True, exist_ok=True)
        self._write_receipt(
            self.journal.with_suffix(".credit.json"),
            json.dumps(
                {
                    "checked_at": datetime.now(UTC).isoformat(),
                    "approved_usd": str(self.limit),
                    "providers": checked,
                },
                sort_keys=True,
            )
            + "\n",
        )

    @staticmethod
    def _write_receipt(path: Path, text: str) -> None:
        with path.open("x", encoding="utf-8") as receipt:
            receipt.write(text)
            receipt.flush()
            os.fsync(receipt.fileno())

    def __enter__(self) -> RequestSession:
        if self._entered:
            raise RuntimeError("a request session cannot be reused")
        self._entered = True
        try:
            self.wallet = self._stack.enter_context(
                RequestWallet(self.journal, campaign=self.campaign, limit=self.limit)
            )
            routes = [
                PricedRoute("/decision/v1/systemone", self.spec.decision, self._keys[self.spec.decision.credential_id])
            ]
            routes.append(
                PricedRoute("/text/chat/completions", self.spec.text, self._keys[self.spec.text.credential_id])
            )
            self.broker = RequestBroker(self.wallet, routes, self.cassette, transport_factory=self._transport_factory)
            self._base = self._stack.enter_context(self.broker)
            self._active = True
            return self
        except BaseException:
            self._stack.close()
            raise

    def __exit__(self, *exc: object) -> None:
        self.stopped = self.stop_reason
        self._active = False
        try:
            self._stack.close()
        finally:
            agent = self.journal.with_suffix(".agent.jsonl.gz")
            judge = self.journal.with_suffix(".judge.jsonl.gz")
            self.cassette.save(agent)
            self.judge_cassette.save(judge)
            credit = self.journal.with_suffix(".credit.json")
            artifacts = (self.journal, agent, judge, *((credit,) if credit.exists() else ()))
            evidence = {
                **self.spec.provenance(),
                "request_runtime": self.runtime,
                "campaign": self.campaign,
                "artifacts": {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in artifacts},
            }
            self._write_receipt(self.journal.with_suffix(".evidence.json"), json.dumps(evidence, sort_keys=True) + "\n")

    @property
    def stop_reason(self) -> str | None:
        reasons = (*self.broker.admission_stops, *self.broker.provider_stops) if self._active else ()
        return self.stopped or (reasons[0] if reasons else None)

    def check_admission(self) -> None:
        if not self._active:
            raise SpendStop("request session is not active")
        if reason := self.stop_reason:
            raise SpendStop(reason)

    def public_json(self, value: JsonValue) -> JsonValue:
        if isinstance(value, str):
            return (
                value.replace(self._base + "/decision", "https://openrouter.ai/api")
                .replace(self._base + "/text", OPENROUTER)
                .replace(self._base, "<request-broker>")
            )
        if isinstance(value, list):
            return [self.public_json(item) for item in value]
        if isinstance(value, dict):
            return {key: self.public_json(item) for key, item in value.items()}
        return value

    def agent_settings(self) -> dict[str, str]:
        self.check_admission()
        return {
            "OPENROUTER_API_KEY": "request-broker-placeholder",
            "FASTBROWSE_DECISION_SOURCE": "openrouter",
            "FASTBROWSE_DECISION_MODEL": self.spec.decision.model,
            "FASTBROWSE_DECISION_BASE_URL": self._base + "/decision",
            "FASTBROWSE_LLM_BASE_URL": self._base + "/text",
            "FASTBROWSE_LLM_API_KEY": "request-broker-placeholder",
            "FASTBROWSE_LLM_MODEL": self.spec.text.model,
            "FASTBROWSE_LLM_PROVIDER": "custom",
        }

    def judge_client(self) -> RequestJudgeClient:
        self.check_admission()
        profile = self.spec.judge
        upstream = _JudgeUpstream(
            self._transport_factory()
            if self._transport_factory
            else httpx.AsyncHTTPTransport(retries=0, trust_env=False)
        )
        proxy = SpendProxy(self.wallet, profile, self._keys[profile.credential_id], transport=upstream)
        return RequestJudgeClient(_JudgeTransport(self, proxy, upstream))
