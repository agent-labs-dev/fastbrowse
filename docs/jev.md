# Jev: what Typesafe documents, and what fastbrowse assumes

The Jev contract used by fastbrowse, checked against Typesafe's documentation for Jev 1.13.
Anything the documentation does not state is marked **ours**: a fastbrowse choice, tuned on the
evals rather than taken from Typesafe.

## The contract

| | Documented | Where fastbrowse relies on it |
|:--|:--|:--|
| Direct API | `POST https://api.typesafe.ai/v1/systemone`, bearer key, body `{model, state, questions}`; response `{model, answers, usage}` ([API](https://docs.typesafe.ai/api), [OpenAPI](https://api.typesafe.ai/openapi.json)) | `clients/typesafe.py`, used when `TYPESAFE_API_KEY` is set, unless `FASTBROWSE_JEV_SOURCE=gateway` |
| Gateway | Model `typesafe-ai/jev` via `https://ai-gateway.vercel.sh/v4/ai/evaluation-model`, body `{state, questions}` ([Gateway](https://vercel.com/docs/ai-gateway/modalities/evaluation), [transport source](https://github.com/vercel/ai/blob/main/packages/gateway/src/gateway-evaluation-model.ts)) | `clients/vercel.py`, used with `AI_GATEWAY_API_KEY`; `FASTBROWSE_JEV_BASE_URL` retargets either |
| Yes/no (Noul) | Direct returns `{type: "noul", noul: P(yes)}`; the gateway returns `probability`. Optional `true`/`false` criteria define the boundary ([Noul](https://docs.typesafe.ai/primitives/noul), [v1 migration](https://docs.typesafe.ai/migrating-to-v1)) | `clients/validation.py` decodes each shape separately; `policy.py` asks one per control to filter a dense page |
| Choice | The top `choice`, every option's probability, and a `confidence` ([Choice](https://docs.typesafe.ai/primitives/choice)) | `policy.py` (operation and target), `retrieval.py` (field and short-fact reads) |
| Score | Ordered levels, returning a probability-weighted index ([Score](https://docs.typesafe.ai/primitives/score)) | Modelled in `jev.py`, not yet called |
| Options | At most 255 per Choice ([Choice](https://docs.typesafe.ai/primitives/choice)) | `MAX_CHOICE_OPTIONS = 255`; the 240 cap and group-then-element selection are **ours** |
| Tokens | 32k for state plus the largest question, 64k in total ([Models](https://docs.typesafe.ai/models)) | `TokenBudget` targets 24k and 48k; the headroom and `chars_per_token = 3.0` are **ours**, because tokens are estimated locally |
| Rate limits | 1,200 requests a minute and 250k tokens a second on the direct API, subject to change ([Models](https://docs.typesafe.ai/models)); no extra gateway limit on paid tiers ([Gateway limits](https://vercel.com/docs/ai-gateway/rate-limits)) | Retries handle 429 responses; concurrent runs share their provider's limits |
| Errors | 400/401/403/404/422/429/5xx, with 529 for overload; retry with backoff, honouring server retry headers ([Exceptions](https://docs.typesafe.ai/sdk/python/api/exceptions)) | `post_with_retry` retries 408, 429, 500, 502, 503, 504 and 529 (not the 4xx request errors) and honours `retry-after-ms` and `retry-after`, capped at 10s |
| Price | $0.042 per million input tokens; output is free ([Models](https://docs.typesafe.ai/models), [gateway catalog](https://ai-gateway.vercel.sh/v1/models)) | `CostComponent.JEV` on the ledger |
| Latency | Provider timings do not establish end-to-end run time | The 1.5s hedge in `clients/validation.py` is **ours**; published run timings are in [evals.md](evals.md) |
| Streaming | Gateway evaluation does not stream ([AI SDK evaluation](https://ai-sdk.dev/docs/ai-sdk-core/evaluation)) | Not needed: answers are a few numbers |

## Provider failover

With both keys set, **ours**: each run starts on `FASTBROWSE_JEV_SOURCE` (direct by default). If a retryable
HTTP status outlasts that provider's retry budget, the client repeats the evaluation through the other
provider and stays there for the rest of the run. A second outage raises; providers never alternate.
Concurrent evaluations already in flight may finish on the first provider.

Request and authentication errors, malformed answers, transport failures without a final retryable HTTP
status, and cancellation do not switch providers. With one key, exhausted retries raise an error.
`FASTBROWSE_JEV_BASE_URL` or a nondefault `FASTBROWSE_JEV_MODEL` disables automatic failover: a backup
must not bypass a proxy or silently replace a pinned model. Default backups use their own public endpoint,
key and model (`jev-1.13.0` direct, `typesafe-ai/jev` through the gateway).

The call's time includes both providers. HTTP errors add no charge; unanswered requests that may have been
billed are estimated from the successful answer's input tokens at Jev's input price, without multiplying
the backup's own retries or hedges. Both routes reach Typesafe, so an outage there can affect both.

## Confidence is not correctness

Typesafe says Jev's probabilities are calibrated: across many answers, frequencies match the stated
probability ([primer](https://docs.typesafe.ai/introduction/machine-learning-primer)). A Choice's `confidence`
summarises how concentrated the distribution is; it is not the chosen option's probability
([Confidence](https://docs.typesafe.ai/confidence)). Noul has no separate confidence: its probability is
the answer.

Every threshold in `config.py` (`recover_below` 0.55, `done_accept_from` 0.85, `claim_problem_above` 0.70,
the 0.90 read cut in `retrieval.py`) is **ours**. None is a Typesafe recommendation. Each is set per
question on the evals, and the gates behind them (VERIFY, the claim check, the LLM fallback) mean a
miscalibrated threshold tends to cost seconds rather than a wrong answer. Not always: a completion
judged at or above `done_accept_from` skips VERIFY, so a confidently wrong DONE is not caught there.

## How the questions are written

Typesafe's guidance ([Primitives](https://docs.typesafe.ai/primitives), [Noul](https://docs.typesafe.ai/primitives/noul),
[Choice](https://docs.typesafe.ai/primitives/choice), [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13)),
and where fastbrowse follows it:

- **Narrow, explicit judgments, with true/false descriptions that match the instruction.** Every Noul
  question in `verification.py` and `safety.py` carries both descriptions.
- **Concrete option boundaries, plus an escape option when coverage is incomplete.** The short-fact read
  offers `synthesis` for the LLM reader and `absent` for no relevant evidence. Scalar field extraction offers
  `none`, and the policy offers `escalate`.
- **Question ids are invisible to the model.** Everything the model needs is in the instruction text.
- **Batch independent questions that share one state; answers cannot see each other.** The policy batches
  operation and target choices, read assessment and applicable sign-in and bot checks. Grouped targets need
  a second choice; code-selected pagination needs no policy call. The done check batches completion,
  unmet actions and whether a draft needs rewriting.
- **Filter before choosing on a dense page.** The Jev 1.13 jaggedness notes say accuracy falls with a large
  state full of irrelevant detail, and suggest a Noul to filter for relevance. Past `max_offered_controls` (160,
  **ours**) the policy asks one Noul per control, packed into requests of about 8k tokens (**ours**) and sent
  together, and offers the highest-scoring controls. A Noul is used rather than ranking one Choice's
  probabilities: a Choice ranks alternatives against each other, and its two-decimal probabilities leave all but
  a handful of 240 options tied at zero. The rubric sits once in the shared state, so each question carries only
  its control. Protected controls skip the check and an unanswered one is kept.
  The short-fact read does the same when a page has more quotable spans than a Choice can offer or more
  text than its input allows: one Noul per window of about 1,500 characters (**ours**), a longer block asked about
  in pieces. Jev chooses only when every window was answered and every one at 0.6 or above (**ours**; Jev scores a passage it cannot place near 0.5) fits in the
  choice; otherwise the LLM reader reads the whole page. An `absent` answer from a narrowed page is not trusted.
  Requests stay small because the gateway sheds large ones: measured on Wikipedia windows, a 26k-token Noul
  batch returned 503 until its retries ran out on five of eight tries, a 4k-token batch on none.
  The step request cannot be split that way, since its state is shared: when one larger than a batch runs out
  of retries, the policy asks it again with the on-screen controls only, then with half of those left. Replayed
  against the gateway, a 27k-token step from a Wikipedia article was answered four times in ten, while requests
  under 10k tokens were answered in all but two of 29 (**ours**).
- **Match state to the question.** Navigation uses the redacted viewport, controls and working notes;
  short-fact selection sees the full capture. Counts and comparisons go to the LLM reader.

## Unused primitives

- **Structured instructions** ([Structure](https://docs.typesafe.ai/primitives/advanced)): `jev.py`
  types instructions as strings. Structured criteria are used where they help: the tab choice passes each tab as an object.
- **Score** for graded judgments such as relevance.
- **Full Choice distributions** for trying a second-best target, rather than only the top pick
  ([hierarchical classification](https://docs.typesafe.ai/cookbooks/hierarchical_classification)).

No faster Jev tier, cross-request cache or logprob access is documented.
