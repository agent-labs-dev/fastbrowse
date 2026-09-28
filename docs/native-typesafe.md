# Native TypeSafe evaluation

Decision for #88, evaluated on 2026-09-28: keep the current client. The native integration provides typed
questions and answers, but its public result loses accounting information that fastbrowse requires to enforce
limits. No dependency or production transport changes are proposed.

The inspected versions were `pydantic-ai-slim[typesafe]==2.51.0` and `typesafe-sdk==0.7.2`, installed in an
isolated environment. The official [TypeSafe model documentation](https://pydantic.dev/docs/ai/models/typesafe/)
and [implementation reference](https://pydantic.dev/docs/ai/api/models/typesafe/) describe the integration.

A transport probe returned HTTP 503 followed by a valid answer carrying 100 input tokens, 2 output tokens and
`usage.cost=0.0123`. Calling `TypeSafeModel.decide` produced two `/v1/systemone` requests and the correct answer.
Its returned `usage.cost` was `None`, and its `DecisionResponse` exposed no request count. This confirms that the
SDK now retries; the original issue's description of a transport without retries is outdated.

| Contract | Native integration | Consequence |
|:--|:--|:--|
| Typed questions and answer distributions | Available through decision-model primitives | Could replace wire parsing |
| Provider-reported dollars | Not copied to the returned usage | Cannot replace `reported_cost` without additional transport accounting |
| Attempts, discarded calls and unknown charges | Absent from the public decision result | Hidden retries would bypass the ledger's request accounting |
| Caller-owned HTTP client | Accepts `httpx2`, while fastbrowse uses `httpx` | Requires another client lifecycle and transport integration |
| Hedging, split-on-size recovery and gateway wire format | Not supplied by this model | Existing transport policy would still be required |

The retry policy can be disabled through an explicitly supplied SDK client. That avoids hidden retries but
does not restore reported cost, hedging or gateway parity. `FallbackModel` routes model failures; it does not
supply fastbrowse's per-attempt ledger or the Vercel gateway's different request format. Replacing only answer
parsing while keeping these layers does not remove enough code to justify another runtime dependency.

The isolated probe used a synthetic key and an in-memory transport; it made no paid or network requests:

```python
import asyncio
import httpx2
from pydantic_ai.models.decision import DecisionRequest, NoulQuestion
from pydantic_ai.models.typesafe import TypeSafeModel
from pydantic_ai.providers.typesafe import TypeSafeProvider
from typesafe_sdk import AsyncTypeSafeClient


async def check():
    attempts = 0

    async def respond(request):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            return httpx2.Response(503, json={"error": {"message": "temporary outage"}})
        return httpx2.Response(
            200,
            json={
                "model": "jev-1.13",
                "answers": {"q": {"type": "noul", "noul": 0.8}},
                "usage": {"input_tokens": 100, "output_tokens": 2, "cost": 0.0123},
            },
        )

    async with httpx2.AsyncClient(transport=httpx2.MockTransport(respond)) as http:
        client = AsyncTypeSafeClient(api_key="synthetic", http_client=http, base_url="https://provider.test")
        model = TypeSafeModel("jev-1.13", provider=TypeSafeProvider(typesafe_client=client))
        result = await model.decide(DecisionRequest(state="green", questions={"q": NoulQuestion()}), {})
        assert attempts == 2 and result.usage.cost is None
        assert not hasattr(result, "requests")


asyncio.run(check())
```

Run it with `uv run --isolated --no-project --with 'pydantic-ai-slim[typesafe]==2.51.0' probe.py`.

No live A/B performance claim is made: a candidate that preserves the accounting contract was not produced,
so the migration fails the compatibility gate before performance testing. Any future replacement must first
preserve cost and attempt accounting, then run `--suite core dev heldout --repeat 3` on both implementations
at equal task versions. Switch only with no regression in pass rate, latency or dollars per run.
