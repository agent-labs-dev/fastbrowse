# fastbrowse

A browser agent for Node.js. It picks one of the controls a page has instead of generating an action, and each
fact in its answer cites a quote from a page it read. This package is the TypeScript SDK. It runs the agent as
a native binary, so the machine needs no Python.

## Install

```sh
npm install fastbrowse
export OPENROUTER_API_KEY=...
```

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
`result.status` before you use the answer.

## The rest

[Use it from JavaScript](https://github.com/agent-labs-dev/fastbrowse#use-it-from-javascript) in the repository
README covers output typed by a Zod or JSON Schema, secrets your own code resolves, the `until` check, step
events, live frames and stopping a run with an `AbortSignal`.

MIT licence.
