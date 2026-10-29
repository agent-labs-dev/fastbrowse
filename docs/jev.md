# Jev: what Typesafe documents, and what fastbrowse assumes

The Jev contract used by fastbrowse, checked against Typesafe's documentation for Jev 1.13. Every route asks Jev 1.14, which keeps 1.13's limits and price.
Anything the documentation does not state is marked **ours**: a fastbrowse choice, tuned on the
evals rather than taken from Typesafe.

## The contract

| | Documented | Where fastbrowse relies on it |
|:--|:--|:--|
| OpenRouter | `POST https://openrouter.ai/api/v1/systemone`, bearer key, TypeSafe request and response shapes, plus `usage.cost` in dollars, `id` and `provider`; bare model ids map to `typesafe/` ([SDK guide](https://openrouter.ai/docs/guides/community/typesafe-sdk)) | `crates/fastbrowse-clients/src/system_one.rs` with base URL `https://openrouter.ai/api`; a supported Jev route, images included, when `OPENROUTER_API_KEY` is set and no direct TypeSafe key is supplied. Not recommended |
| Direct API | `POST https://api.typesafe.ai/v1/systemone`, bearer key, body `{model, state, questions}`; response `{model, answers, usage}` ([API](https://docs.typesafe.ai/api), [OpenAPI](https://api.typesafe.ai/openapi.json)) | `crates/fastbrowse-clients/src/system_one.rs`, selected with `FASTBROWSE_DECISION_SOURCE=typesafe`, or when `TYPESAFE_API_KEY` is set and no source override is supplied. The recommended Jev route |
| Gateway | Model `typesafe-ai/jev` via `https://ai-gateway.vercel.sh/v4/ai/evaluation-model`, body `{state, questions}` ([Gateway](https://vercel.com/docs/ai-gateway/modalities/evaluation), [transport source](https://github.com/vercel/ai/blob/main/packages/gateway/src/gateway-evaluation-model.ts)) | `crates/fastbrowse-clients/src/vercel.rs`, used with `AI_GATEWAY_API_KEY` as a backup for an outage or the last resort when no other Jev key is supplied. A last resort rather than the recommended Jev route, and it receives no Jev image input. `FASTBROWSE_DECISION_BASE_URL` retargets the selected source |
| Yes/no (Noul) | Direct returns `{type: "noul", noul: P(yes)}`; the gateway returns `probability`. Optional `true`/`false` criteria define the boundary ([Noul](https://docs.typesafe.ai/primitives/noul), [v1 migration](https://docs.typesafe.ai/migrating-to-v1)) | `crates/fastbrowse-clients/src/validation.rs` decodes each shape separately; `crates/fastbrowse-agent/src/policy.rs` asks one per control to filter a dense page |
| Choice | The top `choice`, every option's probability, and a `confidence` ([Choice](https://docs.typesafe.ai/primitives/choice)) | `crates/fastbrowse-agent/src/policy.rs` (operation and target), `crates/fastbrowse-agent/src/retrieval/` (field and short-fact reads) |
| Score | Ordered levels, returning a probability-weighted index ([Score](https://docs.typesafe.ai/primitives/score)) | Modelled in `crates/fastbrowse-core/src/decision.rs`, not yet called |
| Options | At most 255 per Choice ([Choice](https://docs.typesafe.ai/primitives/choice)) | `DecisionProfile::jev()` in `crates/fastbrowse-core/src/decision.rs`; group-then-element selection is **ours** |
| Tokens | 32k for state plus the largest question, 64k in total; 262k for both on a request with the `TypeSafe-Danger-Zone-Long-Context: true` header ([Models](https://docs.typesafe.ai/models#long-context)). OpenRouter ([Jev](https://openrouter.ai/docs/guides/community/jev)) and AIMLAPI ([Jev 1.14](https://docs.aimlapi.com/api-references/decision-models/typesafe/jev-1.14)) list Jev at 262,144 tokens; the gateway's catalog lists 32,000 | `TokenBudget` (`crates/fastbrowse-core/src/config.rs`) holds the documented 32k and 64k. TypeSafe publishes no tokenizer, so tokens are estimated locally: a Jev request is counted at 1.5 characters a token (**ours**; recorded dense JSON used 1.84), and an LLM prompt at `chars_per_token = 3.0` (**ours**). Direct TypeSafe sends the long-context header on every request, and it, OpenRouter and AIMLAPI fit a Jev request to 262k: the budget scales with the route's context, so a step's state reaches 393,216 characters, while an LLM prompt keeps the 32k and 64k budget. The gateway keeps 32k and 64k. A step request that does not fit loses its oldest actions, then has its recovery memory, page text and notes cut, each saying what was left out |
| Rate limits | 1,200 requests a minute and 250k tokens a second on the direct API, subject to change ([Models](https://docs.typesafe.ai/models)); no extra gateway limit on paid tiers ([Gateway limits](https://vercel.com/docs/ai-gateway/rate-limits)) | Retries handle 429 responses; concurrent runs share their provider's limits |
| Errors | 400/401/403/404/422/429/5xx, with 529 for overload; retry with backoff, honouring server retry headers ([Exceptions](https://docs.typesafe.ai/sdk/python/api/exceptions)) | `post_with_retry` retries 408, 429, 500, 502, 503, 504, 520 to 524 and 529 (not the 4xx request errors) and honours `retry-after-ms` and `retry-after`, capped at 10s |
| Price | $0.042 per million input tokens; output is free ([Models](https://docs.typesafe.ai/models), [gateway catalog](https://ai-gateway.vercel.sh/v1/models)) | `DecisionProfile::jev()`, and `CostComponent::Jev` on the ledger |
| Latency | Provider timings do not establish end-to-end run time | The retry budget in `crates/fastbrowse-clients/src/validation.rs` is **ours**; published run timings are on the [benchmark page](https://fastbrowse.ai/benchmarks) |
| Streaming | Gateway evaluation does not stream ([AI SDK evaluation](https://ai-sdk.dev/docs/ai-sdk-core/evaluation)) | Not needed: answers are a few numbers |

## Other decision models

Every limit and price above is Jev's, and fastbrowse reads them from the model's `DecisionProfile`
(`crates/fastbrowse-core/src/decision.rs`) rather than from constants. A model's limits bound every request, including
a caller's own `Config`: the choices the policy offers, the group size and shortlist that follow from them, the
token budget, and the questions per request. Its thresholds apply only when the caller passes no `Config`.

A model fastbrowse has no record of, such as one pinned with `FASTBROWSE_DECISION_MODEL`, gets the tightest limits any
documented System One host sets: 26 options a Choice, 64 questions and 64 KiB a request ([System
One](https://pydantic.dev/docs/ai/models/system-one/)). It has no list price, so a call it does not meter is an
unknown cost, which a dollar limit treats as unaffordable. Its thresholds are Jev's, which no eval has checked against
it, and the run's trace says so with a `decision_unvalidated` event. A Jev release other than 1.13 or 1.14 keeps Jev's limits
and price but is unvalidated too.

### Other decision hosts

`FASTBROWSE_DECISION_SOURCE` picks one of these instead of a Jev source. Each is sent only its own key, never
`TYPESAFE_API_KEY`, `OPENROUTER_API_KEY` or `AI_GATEWAY_API_KEY`, and none has a backup: an outage past its
retries ends the run. All but `openai` send System One's body through `crates/fastbrowse-clients/src/system_one.rs`,
AIMLAPI and Perplexity at `/v1/decisions`; `openai`, and `gpt-6-luna` on AIMLAPI, speak OpenAI's Decisions API
through `crates/fastbrowse-clients/src/openai_decisions.rs`.

| Source | Host | Key | Model | Limits |
|:--|:--|:--|:--|:--|
| `telnyx` | `https://api.telnyx.com/v2/ai/typesafe` ([Telnyx](https://developers.telnyx.com/api-reference/decision-models/evaluate-decision-models-typesafe-compatible)) | `TELNYX_API_KEY` | `telnyx/decision-flash`, or `telnyx/decision-pro` | 64 questions a request, 64 options a choice or levels a score; no published price |
| `ollama` | `http://localhost:11434` ([Ollama](https://pydantic.dev/docs/ai/models/system-one/#ollama)) | none, or `FASTBROWSE_DECISION_API_KEY` | `FASTBROWSE_DECISION_MODEL`, such as `nimble` or `tev1` | 26 options, 26 levels, 64 questions and 64 KiB a request |
| `openai` | `https://api.openai.com/v1/decisions` ([Decisions](https://developers.openai.com/api/docs/guides/decisions)) | `OPENAI_API_KEY` | `gpt-6-luna` | 400 questions a request, 2 to 10 levels a score, 255 options a choice; $0.10 per million input tokens, estimated since the response states no cost |
| `perplexity` | `https://api.perplexity.ai/v1/decisions` ([Perplexity](https://docs.perplexity.ai/docs/decisions/quickstart)) | `PERPLEXITY_API_KEY` | `pplx-decider-v1.2`, or `pplx-decider-flash-v1.2` | 128 questions a request, 255 options, 10 levels; $0.02 per million input tokens, $0.01 on flash |
| `aimlapi` | `https://api.aimlapi.com/v1/decisions` ([AIMLAPI](https://docs.aimlapi.com/api-references/decision-models.md)) | `AIMLAPI_API_KEY` | `typesafe/jev-1.14`, or `liquid/d1`, `openai/gpt-6-luna` and later Jev releases | The model's own; AIMLAPI's price is not reported, so every call is an unknown cost |
| `custom` | `FASTBROWSE_DECISION_BASE_URL` | `FASTBROWSE_DECISION_API_KEY`, if it takes one | `FASTBROWSE_DECISION_MODEL` | The profile of a model fastbrowse knows, otherwise the unknown model's |

`FASTBROWSE_DECISION_BASE_URL` also retargets every source above but `custom`, which needs it. Jev 1.14 through
AIMLAPI keeps Jev's thresholds, but its calls are priced as unknown. The Decisions API names a yes/no
question a predicate and has no true and false descriptions, so a Noul's are appended to its instructions; a choice
criterion that is an object is sent as its JSON text, and a refused question fails the request. Telnyx answers an oversized body with HTTP
413, which fastbrowse reads as input too large rather than as an outage. Telnyx says its scores are relative
preference rather than calibrated probability, so no Jev threshold is known to hold on it.

## Choosing a provider

Use direct TypeSafe. It is the recommended Jev route: one service between fastbrowse and the model, the
published direct-API price. Set
`TYPESAFE_API_KEY`; `FASTBROWSE_DECISION_SOURCE=typesafe` makes the choice explicit.

OpenRouter is supported for Jev, images included, when it is the only Jev key, but it is not recommended. It
adds a third party between fastbrowse and TypeSafe. Treat it as a
convenience for a machine that has no direct key, not as the production route.

The Vercel AI Gateway is a last resort rather than the recommended Jev route. `AI_GATEWAY_API_KEY` alone
serves the LLM and also serves Jev as the last resort; beside a direct or OpenRouter key it adds a Jev backup.

Jev images go to TypeSafe's own API and to OpenRouter. On those routes each step's action decision is sent the viewport
screenshot: the state names it `viewport_screenshot`, and the request carries the PNG in TypeSafe's
`assets` map with the `TypeSafe-Danger-Zone-Images` header. A screenshot outside TypeSafe's documented
sizes, or one taken while a secret shows as page text, is not sent. The gateway and every backup
are asked the rest of the state, without the image or its reference. The LLM's own screenshot path is separate; it is governed by the LLM route, not by
this policy.

A click the hit test refuses, because another element is topmost at every point of the control it tried,
is put to Jev's [Box](https://docs.typesafe.ai/primitives/box) on the same route. Jev is shown the viewport and
the control, and asked whether the control is shown with nothing drawn over it, and where it is. Box always
answers with a box, so the click lands at the box's centre only when both answers reach the bar an action's
target is held to (`recover_below`); otherwise the click stays refused. The step's irreversible-action check
runs on that click as on any other.

## Provider failover

Provider selection is **ours**: a supplied `TYPESAFE_API_KEY` selects direct TypeSafe for Jev, the
recommended route. Otherwise `OPENROUTER_API_KEY` is primary, which is supported but not recommended. The
LLM uses `FASTBROWSE_LLM_BASE_URL` when it is set, otherwise OpenRouter when keyed, otherwise the gateway.
`AI_GATEWAY_API_KEY` adds a Vercel AI Gateway backup, or serves Jev when neither other key is supplied. `FASTBROWSE_DECISION_SOURCE=openrouter`, `typesafe` or `gateway` overrides the first provider.
OpenRouter and direct TypeSafe use the gateway as backup when keyed; the gateway uses direct TypeSafe
when keyed, otherwise OpenRouter. A selected source without its key is a configuration error.

If a retryable HTTP status outlasts the first provider's retry budget, the client repeats the evaluation through the backup
provider and stays there for the rest of the run. A second outage raises; providers never alternate.
Concurrent evaluations already in flight may finish on the first provider.

Request and authentication errors, malformed answers, transport failures without a final retryable HTTP
status, and cancellation do not switch providers. Without a configured backup, exhausted retries raise an error.
`FASTBROWSE_DECISION_BASE_URL` or an explicit `FASTBROWSE_DECISION_MODEL` disables automatic failover: a backup
must not bypass a proxy or silently replace a pinned model. Default backups use their own public endpoint,
key and model (`jev-1.14` on OpenRouter, `jev-1.14.0` on direct TypeSafe, `typesafe-ai/jev-1.14.0` through the gateway).

OpenRouter's `usage.cost` and the gateway's reported cost are metered dollars. Missing cost, or $0 with
input tokens, uses Jev's list-price estimate. Direct TypeSafe responses without cost keep that estimate.

The call's time includes both providers. HTTP errors add no charge; unanswered requests that may have been
billed are estimated from the successful answer's input tokens at Jev's input price, without multiplying
the backup's own retries. Cancelled requests without usage retain an input estimate from the
serialized request length divided by `TokenBudget.chars_per_token`, rounded up, at Jev's input price.
This is estimated spend, not provider-reported billing; output is free. Cancellation before dispatch adds
no charge. All three routes reach Typesafe, so an outage there can affect all three.

## Confidence is not correctness

Typesafe says Jev's probabilities are calibrated: across many answers, frequencies match the stated
probability ([primer](https://docs.typesafe.ai/introduction/machine-learning-primer)). A Choice's `confidence`
summarises how concentrated the distribution is; it is not the chosen option's probability
([Confidence](https://docs.typesafe.ai/confidence)). Noul has no separate confidence: its probability is
the answer.

Every threshold in `crates/fastbrowse-core/src/config.rs` (`recover_below` 0.55, `done_accept_from` 0.85, `claim_problem_above` 0.70)
and in `crates/fastbrowse-agent/src/retrieval/` (the 0.90 read cut; a record on a later page of a filtered count or comparison matches from
0.70 and is rejected at 0.20 or below) is **ours**. None is a Typesafe recommendation. Each is set per
question on the evals, and the gates behind them (VERIFY, the claim check, the LLM fallback) mean a
miscalibrated threshold tends to cost seconds rather than a wrong answer. Not always: a completion
judged at or above `done_accept_from` skips VERIFY, so a confidently wrong DONE is not caught there.

## How the questions are written

Typesafe's guidance ([Primitives](https://docs.typesafe.ai/primitives), [Noul](https://docs.typesafe.ai/primitives/noul),
[Choice](https://docs.typesafe.ai/primitives/choice), [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13)),
and where fastbrowse follows it:

- **Narrow, explicit judgments, with true/false descriptions that match the instruction.** Every Noul
  question in `crates/fastbrowse-agent/src/verification/` and `crates/fastbrowse-core/src/safety.rs` carries both descriptions.
- **Concrete option boundaries, plus an escape option when coverage is incomplete.** The short-fact read
  offers `synthesis` for the LLM reader and `absent` for no relevant evidence. Scalar field extraction offers
  `none`, and the policy offers `escalate`. Repeated short values from the same page and frame share one option
  with every source context, so identical answers do not compete as separate choices.
- **Question ids are invisible to the model.** Everything the model needs is in the instruction text.
- **Batch independent questions that share one state; answers cannot see each other.** The policy batches
  operation and target choices, read assessment and applicable sign-in and bot checks. Grouped targets need
  a second choice; code-selected pagination needs no policy call. The done check batches completion,
  unmet actions and whether a draft needs rewriting.
- **Filter before choosing on a dense page.** The Jev 1.13 jaggedness notes say accuracy falls with a large
  state full of irrelevant detail, and suggest a Noul to filter for relevance. When a page has more controls than
  one group step can offer (`group_size` times the model's options per Choice) or its request does not fit Jev's
  limit, the policy asks one Noul per control, packed into requests of about 8k tokens (**ours**) and sent
  together, and offers the highest-scoring controls. A Noul is used rather than ranking one Choice's
  probabilities: a Choice ranks alternatives against each other, and its two-decimal probabilities leave all but
  a handful of 255 options tied at zero. The rubric sits once in the shared state, so each question carries only
  its control. Protected controls skip the check and an unanswered one is kept.
  The short-fact read does the same when a page has more quotable spans than a Choice can offer or more
  text than its input allows: one Noul per window of about 1,500 characters (**ours**), a longer block asked about
  in pieces. Jev chooses only when every window was answered and every one at 0.6 or above (**ours**; Jev scores a passage it cannot place near 0.5) fits in the
  choice; otherwise the LLM reader reads the whole page. An `absent` answer from a narrowed page is not trusted.
  Requests stay small because the gateway sheds large ones: measured on Wikipedia windows, a 26k-token Noul
  batch returned 503 until its retries ran out on five of eight tries, a 4k-token batch on none.
  The step request cannot be split that way, since its state is shared: when one larger than a batch runs out
  of retries, the policy asks it again once with the on-screen controls only, and past that reports the outage. Replayed
  against the gateway, a 27k-token step from a Wikipedia article was answered four times in ten, while requests
  under 10k tokens were answered in all but two of 29 (**ours**).
- **Match state to the question.** Navigation uses the redacted viewport, controls and working notes;
  short-fact selection sees the full capture. Counts and comparisons go to the LLM reader.

## Unused primitives

- **Structured instructions** ([Structure](https://docs.typesafe.ai/primitives/advanced)): `crates/fastbrowse-core/src/decision.rs`
  types instructions as strings. Structured criteria are used where they help: the tab choice passes each tab as an object.
- **Score** for graded judgments such as relevance.
- **Full Choice distributions** for trying a second-best target, rather than only the top pick
  ([hierarchical classification](https://docs.typesafe.ai/cookbooks/hierarchical_classification)).

No faster Jev tier, cross-request cache or logprob access is documented.
