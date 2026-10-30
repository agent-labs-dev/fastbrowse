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

The binary brings no browser. `local: true` drives the Chrome on your machine. Without it a run starts a
Browser Use Cloud browser, which needs `BROWSER_USE_API_KEY`.

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

`run` rejects when no run took place or none finished: with `RpcError` when the server refuses the request before
a browser opens (a bad option, a missing key, a busy server), with `AbortError` when `signal` stopped the run, and
with `ProcessExitedError` when the process is gone. It also rejects with `OutputValidationError` when a run
finished but its data fails the `output` schema. A run that fails after it starts, including one whose
callback throws, resolves with status `error`.

## Options

The result is the `RunResult` the Python library returns. Its fields keep their Python names, such as
`final_url`, since the types are generated from the protocol's schema; the options are camelCase.

`output` is `data` after a Zod (4.2 or newer) or ArkType schema's validation and transforms, or a Valibot schema
wrapped by `@valibot/to-json-schema`, typed as the schema's output. Data the schema refuses rejects with
`OutputValidationError`, which carries the result. A plain JSON Schema object is accepted too; `output` is then
`data`, typed `unknown`. The server accepts nested objects, arrays, `enum`, `const` and optional values and refuses
any other keyword by name, but a run fills only a flat object of required string, number, integer and boolean
fields. With any other field it ends `unverified` with no data.

`run` also takes `context`, `inputs`, `attachments` as bytes, `downloads`, `record`, `onFrame` (JPEG frames of the
active tab; none are sent without it), and the browser choices `chrome`, `cloudProfile`, `cdpUrl`, `cdpPort`,
`attach`, `targetMatch`, `foreground`, `proxyCountry` and `viewport`. `Fastbrowse.start`
takes the browser choices as defaults for every run, and a run passes `null` for one to go without that default.
`env` in `Fastbrowse.start` adds to the environment the process inherits.

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
