# LLM providers

The LLM plans, reads and writes. By default it goes to OpenRouter when `OPENROUTER_API_KEY` is set, otherwise to
the Vercel AI Gateway, and `FASTBROWSE_LLM_BASE_URL` points it at any other OpenAI-compatible chat completions
endpoint instead. `FASTBROWSE_LLM_PROVIDER` names the provider outright and fills in the rest:

```sh
export FASTBROWSE_LLM_PROVIDER=openai
export OPENAI_API_KEY=...
export FASTBROWSE_LLM_MODEL=...      # the model id the provider serves
```

Jev still needs its own key ([jev.md](jev.md)); the provider choice is the LLM's alone.

## What each preset sends

Every preset but `anthropic` speaks the provider's OpenAI-compatible chat completions API; `anthropic` speaks the
Messages API ([below](#claude-through-the-messages-api)). A preset fixes the address, the variable its key is read
from, how the schema is asked for and where a reasoning effort goes. The table is
[providers.rs](../crates/fastbrowse-clients/src/providers.rs).

| `FASTBROWSE_LLM_PROVIDER` | Address | Key | Schema | Reasoning effort | Default model |
|:--|:--|:--|:--|:--|:--|
| `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | strict `json_schema` | `reasoning.effort`, `low` unless set | `google/gemini-3.8-flash` |
| `gateway` | `https://ai-gateway.vercel.sh/v1` | `AI_GATEWAY_API_KEY` | strict `json_schema` | `reasoning.effort`, `low` unless set | `google/gemini-3.8-flash` |
| `openai` | `https://api.openai.com/v1` | `OPENAI_API_KEY` | strict `json_schema` | `reasoning_effort` | none |
| `anthropic` | `https://api.anthropic.com/v1/messages` | `ANTHROPIC_API_KEY`, as `x-api-key` | structured outputs (`output_config.format`) | `output_config.effort`, `low` unless set | `claude-sonnet-5-5`, `claude-haiku-5-5` for field text and shortcuts |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta/openai` | `GEMINI_API_KEY` | strict `json_schema` | `reasoning_effort` | none |
| `groq` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` | strict `json_schema` | `reasoning_effort` | `openai/gpt-oss-120b` |
| `together` | `https://api.together.ai/v1` | `TOGETHER_API_KEY` | `json_schema`, not strict | not taken | none |
| `mistral` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` | `json_schema`, not strict | `reasoning_effort` | none |
| `deepseek` | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` | `json_object`, schema in the prompt | not taken | none |
| `xai` | `https://api.x.ai/v1` | `XAI_API_KEY` | strict `json_schema` | `reasoning_effort` | none |
| `fireworks` | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` | `json_schema`, not strict | not taken | none |
| `azure` | your resource's `/openai/v1`, in `FASTBROWSE_LLM_BASE_URL` | `AZURE_OPENAI_API_KEY`, as `api-key` | strict `json_schema` | `reasoning_effort` | none: name your deployment |
| `ollama` | `http://localhost:11434/v1` | `FASTBROWSE_LLM_API_KEY`, optional | strict `json_schema` | `reasoning_effort` | none |
| `lmstudio` | `http://localhost:1234/v1` | `FASTBROWSE_LLM_API_KEY`, optional | strict `json_schema` | `reasoning.effort` | none |
| `vllm` | `http://localhost:8000/v1` | `FASTBROWSE_LLM_API_KEY`, optional | strict `json_schema` | `reasoning.effort` | none |
| `custom` | `FASTBROWSE_LLM_BASE_URL`, required | `FASTBROWSE_LLM_API_KEY`, optional | strict `json_schema` | `reasoning.effort` | none |

Rules that hold across the table:

- **A key goes to its own provider's host and nowhere else.** A hosted preset reads only its own variable, so
  `OPENAI_API_KEY` is never sent to Groq, and `OPENROUTER_API_KEY` never leaves OpenRouter. A hosted preset refuses
  `FASTBROWSE_LLM_BASE_URL`, since that would carry its key to another address; use `custom` for a proxy, with
  `FASTBROWSE_LLM_API_KEY`. `azure` takes an address only on `.openai.azure.com` or `.services.ai.azure.com` over
  https.
- **A model is named, or the run does not start.** Without a default, the run stops before its first request and
  asks for `FASTBROWSE_LLM_MODEL`: an OpenRouter id sent elsewhere would fail as a 404 on the first plan instead.
  `FASTBROWSE_LLM_MODEL_<PURPOSE>` pins one purpose as it does on OpenRouter.
- **The reasoning effort is sent only when `FASTBROWSE_LLM_REASONING` is set**, except on OpenRouter, the
  gateway and Anthropic, which default to `low`. A provider whose OpenAI-compatible API takes no effort stops the run with a
  configuration error, rather than drop a setting you asked for.
- **`FASTBROWSE_LLM_STRICT` overrides the preset's strictness, and `FASTBROWSE_LLM_EXTRA_BODY` applies to every
  preset but OpenRouter and the gateway.** The extra body may not set a field fastbrowse sends itself, including
  `max_completion_tokens` and `reasoning_effort` where the preset sends them.
- **Where the schema is in the prompt, it is not enforced.** fastbrowse validates every reply against the schema
  and asks once for a correction, as it does everywhere, so a reply that drifts costs a second call.
- **Cost comes from what the provider reports.** OpenRouter and the gateway report dollars. The others report
  tokens, so their calls are recorded with tokens and an unknown cost, never zero, and `--max-dollars` stops the
  run after the first such call.

## Claude through the Messages API

Anthropic's OpenAI-compatible endpoint ignores `response_format`, so a schema sent there is not enforced. The
`anthropic` preset uses the Messages API instead, with the schema as structured output, and reads each reply through
the same retries, schema repair, truncation retry and ledger as every other LLM call.

- The key goes in `x-api-key`, with `anthropic-version: 2023-06-01`. System messages become the top-level `system`,
  and screenshots are base64 image blocks.
- Structured outputs refuse some schema keywords with HTTP 400: numeric bounds, string lengths, `minItems` above 1
  and the rest of [the list](https://platform.claude.com/docs/en/build-with-claude/structured-outputs). They are
  left out of the schema sent, and the reply is still validated against the full schema, so a value outside a
  dropped bound is repaired as any other schema miss is.
- Claude 5 models think by default, and thinking counts against the output cap, so the effort is always sent: `low`
  unless `FASTBROWSE_LLM_REASONING` says otherwise. `none` is refused, since not every Claude model can have its
  thinking turned off. A reply cut at the cap is asked for again with four times the room, as on every route.
- A refusal (`stop_reason: refusal`) is asked once more and then reported as an LLM failure.
- The response reports tokens, cached input included, but no dollars, so the cost is unknown as on the other direct
  providers.

No other provider has a native adapter. Gemini's and xAI's OpenAI-compatible endpoints take a `json_schema`
`response_format`, and Azure OpenAI's v1 API takes the OpenAI request shape at your resource, so those are presets.
Amazon Bedrock is not supported: its Converse API signs each request with AWS Signature Version 4 from the AWS
credential chain, which this client does not carry. Reach Claude on Bedrock through `custom` and a gateway that
holds the AWS credentials, or use `anthropic`.

## Sources

Each preset was checked against the provider's own documentation on 2026-10-28.

- **OpenAI.** [Chat completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create):
  `https://api.openai.com/v1/chat/completions` with `Authorization: Bearer`, `response_format` of type
  `json_schema` with `strict`, `reasoning_effort` from `none` to `max`, and `max_tokens` deprecated in favour of
  `max_completion_tokens`, which o-series models require.
- **Anthropic.** [OpenAI SDK compatibility](https://platform.claude.com/docs/en/api/openai-sdk): base URL
  `https://api.anthropic.com/v1/`, `response_format` and `reasoning_effort` are ignored, and the page points to the
  native API's structured outputs for guaranteed schema conformance.
  [Structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs):
  `output_config.format` of type `json_schema` with no beta header, `additionalProperties: false` on every object,
  the keywords that answer HTTP 400, and `stop_reason` `max_tokens` and `refusal`.
- **Gemini.** [OpenAI compatibility](https://ai.google.dev/gemini-api/docs/openai): base URL
  `https://generativelanguage.googleapis.com/v1beta/openai/`, `Authorization: Bearer` with the Gemini key,
  structured output through `response_format`, and `reasoning_effort` (`none` only on 2.5 models).
- **Groq.** [Structured outputs](https://console.groq.com/docs/structured-outputs) and
  [reasoning](https://console.groq.com/docs/reasoning): `https://api.groq.com/openai/v1` with
  `Authorization: Bearer`, strict `json_schema` on `openai/gpt-oss-20b`, `openai/gpt-oss-120b` and a few others,
  best-effort on the rest, and `reasoning_effort` per model.
- **Together AI.** [JSON mode](https://docs.together.ai/docs/json-mode): `https://api.together.ai/v1/chat/completions`
  with `Authorization: Bearer`, `response_format` of type `json_schema`, no strict flag or effort documented.
- **Mistral.** [Chat completions reference](https://docs.mistral.ai/api/endpoint/chat):
  `https://api.mistral.ai/v1/chat/completions` with `Authorization: Bearer`, `response_format` of type
  `json_schema`, and `reasoning_effort` from `none` to `xhigh`.
- **DeepSeek.** [JSON output](https://api-docs.deepseek.com/guides/json_mode): base URL `https://api.deepseek.com`
  and `response_format` of type `json_object` only, with the word "json" required in the prompt.
- **xAI.** [Structured outputs](https://docs.x.ai/docs/guides/structured-outputs) and
  [reasoning](https://docs.x.ai/docs/guides/reasoning): `https://api.x.ai/v1`, `json_schema` with `strict: true`,
  and `reasoning_effort` on grok-4.5 and later.
- **Fireworks.** [Structured responses](https://docs.fireworks.ai/structured-responses/structured-response-formatting):
  `https://api.fireworks.ai/inference/v1`, `response_format` of type `json_schema`, no strict flag documented.
- **Azure OpenAI.** [v1 API](https://learn.microsoft.com/en-us/azure/ai-foundry/openai/api-version-lifecycle):
  `https://YOUR-RESOURCE-NAME.openai.azure.com/openai/v1/` or `.services.ai.azure.com`, no `api-version`, and an
  `api-key` header for key authentication. The model is your deployment's name.
- **Ollama.** [OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility):
  `http://localhost:11434/v1/`, a key the server ignores, `response_format`, and `reasoning_effort`.
- **LM Studio.** [Structured output](https://lmstudio.ai/docs/developer/openai-compat/structured-output):
  `http://localhost:1234/v1` and `response_format` of type `json_schema`.
- **vLLM.** [Structured outputs](https://docs.vllm.ai/en/latest/features/structured_outputs.html):
  `http://localhost:8000/v1` and `response_format` of type `json_schema`, on by default.
