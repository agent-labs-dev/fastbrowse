# fastbrowse

A browser agent for Node.js. It picks one of the controls a page has instead of generating an action, and each
fact in its answer cites a quote from a page it read. This package is the TypeScript SDK. It runs the agent as
a native binary, so the machine needs no Python.

## Install

```sh
npm install fastbrowse
export OPENROUTER_API_KEY=...
```

`OPENROUTER_API_KEY` is the recommended route for Jev and the LLM. Without it, the LLM uses `AI_GATEWAY_API_KEY`
and Jev uses `TYPESAFE_API_KEY` when set.

With it npm installs the binary for your platform as an optional dependency: `@fastbrowse/darwin-arm64`,
`darwin-x64`, `linux-arm64`, `linux-x64` or `win32-x64`. No install script runs and nothing is downloaded on
first use. It needs Node.js 20 or newer, and on Linux it needs glibc.

The binary brings no browser. `local: true` drives your own Chrome through the
[fastbrowse extension](../../docs/cli.md#your-own-chrome), signed in as you are; add `launch: true` to start a separate
Chrome instead. Without either a run starts a Browser Use Cloud browser, which needs `BROWSER_USE_API_KEY`.

## First run

```ts
import { Fastbrowse } from 'fastbrowse';

const fb = await Fastbrowse.start({ local: true });
try {
  const result = await fb.run('Find the httpx package and report its latest released version.', {
    start: 'https://pypi.org/',
    limits: { maxDollars: 0.1 },
  });
  console.log(result.status, result.answer);
  for (const evidence of result.evidence) console.log(`  "${evidence.quote}" from ${evidence.url}`);
} finally {
  await fb.close();
}
```

`run` resolves with the result whatever status the run ended in, and only `complete` is success. Read
`result.status` before you use the answer. A `needs_login` result, and a `blocked` one at a bot check, says what
stopped it in `result.wall`: `kind` is `password`, `one_time_code`, `sign_in` or `bot_check`, with the page's
`origin`, the labels of the `fields` it asked for and `seen_at_ms`.

## Processes and errors

`Fastbrowse.start` starts one fastbrowse process and `run` sends it a task. The process serves one run at a time:
a second `run` while one is active rejects with the busy error, so start more instances to run side by side.
`close()` shuts the process down and waits for it, and the process also exits when yours does.

`Fastbrowse.start({ inProcess: true })` runs on the native addon in your Node process. PDF reading still uses a
helper binary, selected by `binaryPath`, then `FASTBROWSE_BINARY`, then the installed platform package. Keep the
addon and helper on matching fastbrowse versions.

`run` rejects when no run took place or none finished: with `RpcError` when the server refuses the request before
a browser opens (a bad option, a missing key, a busy server), with `AbortError` when `signal` stopped the run, and
with `ProcessExitedError` when the process is gone. It also rejects with `OutputValidationError` when a run
finished but its data fails the `output` schema. A run that fails after it starts, including one whose
callback throws, resolves with status `error`.

Jev cost lines include `decision_purpose` when the agent named the decision: `action`, `authorization`,
`relevance`, `read`, `verify` or `locate`. LLM lines keep their existing `purpose`. An unscoped decision
client call omits the optional field, and older receipts remain readable.

An `AbortError` carries `receipt` once the server has closed the browser: the steps it reported, known cost
lines and finished recordings. A cancelled `RpcError` carries the same receipt. A cancellation before the run
starts has none. The receipt has no status or answer, and an unpriced charge remains `unknown`.

## Options

The result is the `RunResult` the Python library returns. Its fields keep their Python names, such as
`final_url`, since the types are generated from the protocol's schema; the options are camelCase.

A safe final screenshot is returned in `result.artifacts` with `kind: "screenshot"`, `mime_type: "image/png"`,
a SHA-256 digest and a base64 data URI in `uri`. A task can request the screenshot without enabling step frames.
If a resolved secret is visible, the image is withheld.

`output` is `data` after a Zod (4.2 or newer) or ArkType schema's validation and transforms, or a Valibot schema
wrapped by `@valibot/to-json-schema`, typed as the schema's output. Data the schema refuses rejects with
`OutputValidationError`, which carries the result. A plain JSON Schema object is accepted too; `output` is then
`data`, typed `unknown`. The server accepts nested objects, arrays, `enum`, `const` and optional values and refuses
any other keyword by name, but a run fills only a flat object of required string, number, integer and boolean
fields. With any other field it ends `unverified` with no data.

`run` also takes `context`, `inputs`, `attachments` as bytes, `downloads`, `record`, `recipe`, `saveRecipe`, `onFrame` (JPEG frames of the
active tab; none are sent without it), and the browser choices `chrome`, `cloudProfile`, `cdpUrl`, `cdpPort`,
`attach`, `targetMatch`, `foreground`, `proxyCountry`, `viewport`, `cloudExtensions` and `cloudTimeoutMinutes`.
`Fastbrowse.start` takes the browser choices as defaults for every run, and a run passes `null` for one to go
without that default.

A run reads pages over HTTP until the task needs a browser. `fetch: true` never starts one, and `browser: true`
starts one from the first step; the server refuses the two together. `allowedOrigins` limits the documents a run
may open, read and control to those exact origins: navigations, redirects, frames and popups elsewhere are refused
before they are sent, so a sign-in that redirects through another origin needs it listed too. It scopes documents,
not the network, and a scoped run sends no frames and cannot `record`.
`env` in `Fastbrowse.start` adds to the environment the process inherits.
Recipe paths name files on the server running fastbrowse. `saveRecipe` writes after a completed run; a write failure is logged without changing the task status, so check the file before replay;
`recipe` reuses saved values for matching live actions with the same task.

## Platforms

There is no binary for Alpine or another musl system, and none for a platform outside the five. There
`Fastbrowse.start` rejects with an error that names the platform; name a binary of your own with `binaryPath` or
`FASTBROWSE_BINARY`. The macOS binaries carry an ad-hoc signature and are not notarized, which a Mac that allows
programs only by the team that signed them refuses. The Windows binary is not signed. Bun and Deno are untested.

## The rest

[Use it from JavaScript](https://github.com/agent-labs-dev/fastbrowse#use-it-from-javascript) in the repository
README has a fuller example: output typed by a Zod schema, secrets your own code resolves, the `until` check,
step events and stopping a run with an `AbortSignal`.

MIT licence.
