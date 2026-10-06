import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { AbortError, Fastbrowse, FastbrowseError, RpcError, type RunOptions } from '../src/index.ts';
import { scriptedRun } from './server.ts';

// The code `ErrorCode` gives it in `src/fastbrowse/protocol.py`.
const CANCELLED = -32002;

// A browser the caller already runs, so no run is refused for a missing Chrome. Nothing connects to it.
const ownBrowser = { cdpUrl: 'ws://127.0.0.1:9222/devtools/browser/none' };

async function withServer<T>(use: (fb: Fastbrowse) => Promise<T>): Promise<T> {
  const fb = await Fastbrowse.start(scriptedRun('until_cancelled'));
  try {
    return await use(fb);
  } finally {
    await fb.close();
  }
}

/** A run that goes on until it is cancelled, and the file that is there once its browser has closed. */
function endless(): { options: RunOptions; closed: () => boolean } {
  const marker = join(mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-')), 'closed');
  return { options: { ...ownBrowser, inputs: { closed: marker } }, closed: () => existsSync(marker) };
}

/** A run under way: its promise, and a promise for the moment its browser is open. */
function begin(fb: Fastbrowse, options: RunOptions): { run: Promise<unknown>; started: Promise<void> } {
  let onEvent = () => {};
  const started = new Promise<void>(resolve => {
    onEvent = resolve;
  });
  const run = fb.run('Wait for the sale to open', { ...options, onEvent });
  // The test awaits `started` first, and a rejection in that time must not count as unhandled.
  run.catch(() => {});
  return { run, started };
}

/** How many `run` requests the server has read, this one included. */
async function runsAsked(fb: Fastbrowse): Promise<unknown> {
  return (await fb.run('Count the runs', ownBrowser)).data;
}

test('aborting mid-run rejects with an AbortError once the browser of the run has closed', async () => {
  await withServer(async fb => {
    const { options, closed } = endless();
    const stop = new AbortController();
    const { run, started } = begin(fb, { ...options, signal: stop.signal });
    await started;
    assert.equal(closed(), false);

    stop.abort();
    await assert.rejects(run, (error: unknown) => {
      assert.ok(error instanceof AbortError);
      assert.ok(error instanceof FastbrowseError);
      assert.equal(error.name, 'AbortError');
      return true;
    });
    assert.equal(closed(), true);
  });
});

test('a signal that aborted before the call rejects with its reason as the cause, and starts no run', async () => {
  await withServer(async fb => {
    const reason = new Error('the user pressed stop');
    await assert.rejects(fb.run('Add the kettle to the cart', { ...ownBrowser, signal: AbortSignal.abort(reason) }), {
      name: 'AbortError',
      cause: reason,
    });
    assert.equal(await runsAsked(fb), 1);
  });
});

test('a signal that aborts as the run is sent cancels it', async () => {
  await withServer(async fb => {
    const stop = new AbortController();
    const run = fb.run('Wait for the sale to open', { ...endless().options, signal: stop.signal });
    stop.abort();
    await assert.rejects(run, AbortError);
  });
});

test('AbortSignal.timeout ends a run that outlasts it, and the cause says it was a timeout', async () => {
  await withServer(async fb => {
    const { options, closed } = endless();
    const { run, started } = begin(fb, { ...options, signal: AbortSignal.timeout(500) });
    await started;
    await assert.rejects(run, (error: unknown) => {
      assert.ok(error instanceof AbortError);
      assert.equal((error.cause as Error).name, 'TimeoutError');
      return true;
    });
    assert.equal(closed(), true);
  });
});

test('a run after an aborted one on the same instance succeeds', async () => {
  await withServer(async fb => {
    const stop = new AbortController();
    const { run, started } = begin(fb, { ...endless().options, signal: stop.signal });
    await started;
    stop.abort();
    await assert.rejects(run, AbortError);

    const next = await fb.run('Add the kettle to the cart', { ...ownBrowser, signal: new AbortController().signal });
    assert.equal(next.status, 'complete');
  });
});

test('aborting after the run has resolved does nothing', async () => {
  await withServer(async fb => {
    const stop = new AbortController();
    const result = await fb.run('Add the kettle to the cart', { ...ownBrowser, signal: stop.signal });
    assert.equal(result.status, 'complete');

    stop.abort();
    // The run under way now is not the one the signal was for.
    const { options, closed } = endless();
    const { run, started } = begin(fb, options);
    await started;
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(closed(), false);

    await fb.close();
    await assert.rejects(run);
  });
});

test('close during a run rejects it with the cancelled error of the server, once its browser has closed', async () => {
  const fb = await Fastbrowse.start(scriptedRun('until_cancelled'));
  const { options, closed } = endless();
  const { run, started } = begin(fb, { ...options, signal: new AbortController().signal });
  await started;

  await fb.close();
  await assert.rejects(run, (error: unknown) => {
    assert.ok(error instanceof RpcError);
    assert.equal(error.code, CANCELLED);
    return true;
  });
  assert.equal(closed(), true);
});
