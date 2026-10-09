import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  type BrowserEvent,
  Fastbrowse,
  FastbrowseError,
  ProcessExitedError,
  RpcError,
  type RunOptions,
  type RunResult,
  type StartOptions,
  type Status,
  type StepEvent,
} from '../src/index.ts';
import { scriptedRun } from './server.ts';

// The codes `ErrorCode` gives them in `src/fastbrowse/protocol.py`.
const INVALID_PARAMS = -32602;
const BUSY = -32001;
const CONFIGURATION = -32003;

// A browser the caller already runs, which is the one choice a run is not refused for on a machine with no
// Chrome and no cloud key. Nothing connects to it: the scripted runs open no browser.
const ownBrowser = { cdpUrl: 'ws://127.0.0.1:9222/devtools/browser/none' };

async function withServer<T>(options: StartOptions, use: (fb: Fastbrowse) => Promise<T>): Promise<T> {
  const fb = await Fastbrowse.start(options);
  try {
    return await use(fb);
  } finally {
    await fb.close();
  }
}

function withResolvers(): { promise: Promise<void>; resolve: () => void } {
  let resolve = () => {};
  const promise = new Promise<void>(settle => {
    resolve = settle;
  });
  return { promise, resolve };
}

interface Echo {
  params: Record<string, unknown>;
  attachments: number[][];
}

/** What the server was sent for a run with these options: the params of the `run` request, as written. */
async function sent(fb: Fastbrowse, options: RunOptions): Promise<Record<string, unknown>> {
  const result = await fb.run('Add the kettle to the cart', options);
  const { run_id: runId, ...params } = (result.data as Echo).params;
  assert.equal(typeof runId, 'string');
  return params;
}

test('run resolves with the result of the run', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    const result: RunResult = await fb.run('Add the kettle to the cart', ownBrowser);
    const status: Status = result.status;
    assert.equal(status, 'complete');
    assert.equal(result.answer, 'Add the kettle to the cart');
    assert.deepEqual(result.steps, []);
  });
});

test('each option reaches the server under its wire name with its value intact', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    const onLocalChrome = await sent(fb, {
      start: 'https://shop.example/',
      inputs: { size: 'large', deliveryNote: 'Leave it with a neighbour' },
      limits: { maxSteps: 5, maxJevCalls: 7, maxLlmCalls: 3, maxDollars: 0.5, maxSeconds: 30 },
      authorization: { irreversibleActions: true },
      downloads: '/tmp/fastbrowse-downloads',
      record: '/tmp/fastbrowse-downloads/run.mp4',
      cursor: true,
      local: true,
      chrome: { binary: process.execPath, headed: true, profile: '/tmp/fastbrowse-profile' },
      viewport: [1280, 720],
      cloudAllowResizing: true,
    });
    assert.deepEqual(onLocalChrome, {
      task: 'Add the kettle to the cart',
      start: 'https://shop.example/',
      inputs: { size: 'large', deliveryNote: 'Leave it with a neighbour' },
      limits: { max_steps: 5, max_jev_calls: 7, max_llm_calls: 3, max_dollars: 0.5, max_seconds: 30 },
      authorization: { irreversible_actions: true },
      downloads: '/tmp/fastbrowse-downloads',
      record: '/tmp/fastbrowse-downloads/run.mp4',
      cursor: true,
      local: true,
      chrome: { binary: process.execPath, headed: true, profile: '/tmp/fastbrowse-profile' },
      viewport: [1280, 720],
      cloud_allow_resizing: true,
    });

    const attached = await sent(fb, { ...ownBrowser, attach: true, targetMatch: 'Checkout' });
    assert.deepEqual(attached, {
      task: 'Add the kettle to the cart',
      cdp_url: ownBrowser.cdpUrl,
      attach: true,
      target_match: 'Checkout',
    });

    assert.deepEqual(await sent(fb, { cdpPort: 9222 }), { task: 'Add the kettle to the cart', cdp_port: 9222 });

    assert.deepEqual(await sent(fb, { cloudProfile: 'signed-in', proxyCountry: 'de' }), {
      task: 'Add the kettle to the cart',
      cloud_profile: 'signed-in',
      proxy_country: 'de',
    });
  });
});

test('browser defaults given to start are part of every run, and a per-run option wins', async () => {
  const defaults = { local: true, chrome: { binary: process.execPath }, viewport: [800, 600] as [number, number] };
  await withServer({ ...scriptedRun('echo'), ...defaults }, async fb => {
    assert.deepEqual(await sent(fb, {}), {
      task: 'Add the kettle to the cart',
      local: true,
      chrome: { binary: process.execPath },
      viewport: [800, 600],
    });
    assert.deepEqual(await sent(fb, { viewport: [1024, 768], start: 'https://shop.example/' }), {
      task: 'Add the kettle to the cart',
      start: 'https://shop.example/',
      local: true,
      chrome: { binary: process.execPath },
      viewport: [1024, 768],
    });
  });
});

test('a per-run option left undefined leaves the default from start in place', async () => {
  await withServer({ ...scriptedRun('echo'), cdpPort: 9222 }, async fb => {
    // What a caller without `exactOptionalPropertyTypes` can write, and what `{ cdpPort: maybePort }` comes to.
    const options = { cdpPort: undefined as unknown as number };
    assert.deepEqual(await sent(fb, options), { task: 'Add the kettle to the cart', cdp_port: 9222 });
  });
});

test('a per-run null takes a default from start out of that run, and the next run has it again', async () => {
  const defaults = { cdpPort: 9222, attach: true, viewport: [800, 600] as [number, number] };
  await withServer({ ...scriptedRun('echo'), ...defaults }, async fb => {
    // The server refuses `cdp_port` beside `cdp_url`, so without the null this run could not name its own browser.
    assert.deepEqual(await sent(fb, { cdpPort: null, attach: null, ...ownBrowser }), {
      task: 'Add the kettle to the cart',
      cdp_url: ownBrowser.cdpUrl,
      viewport: [800, 600],
    });
    assert.deepEqual(await sent(fb, {}), {
      task: 'Add the kettle to the cart',
      cdp_port: 9222,
      attach: true,
      viewport: [800, 600],
    });
  });
});

test('a null for an option start gave no default for sends nothing', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    const params = await sent(fb, { ...ownBrowser, chrome: null, proxyCountry: null });
    assert.deepEqual(params, { task: 'Add the kettle to the cart', cdp_url: ownBrowser.cdpUrl });
  });
});

test('an attachment passed as bytes arrives intact', async () => {
  // Every byte value, so one the encoding mangles is one the run would not get back.
  const content = Uint8Array.from({ length: 256 }, (_, byte) => byte);
  await withServer(scriptedRun('echo'), async fb => {
    const result = await fb.run('Upload the scan', {
      ...ownBrowser,
      attachments: [{ name: 'scan.bin', mimeType: 'application/octet-stream', content }],
    });
    const { params, attachments } = result.data as Echo;
    assert.deepEqual(attachments, [Array.from(content)]);
    const [attachment] = params.attachments as Record<string, unknown>[];
    assert.equal(attachment?.name, 'scan.bin');
    assert.equal(attachment?.mime_type, 'application/octet-stream');
  });
});

test('onEvent receives the browser event and every step event, in order and before run resolves', async () => {
  await withServer(scriptedRun('three_events'), async fb => {
    const events: (StepEvent | BrowserEvent)[] = [];
    await fb.run('Add the kettle to the cart', { ...ownBrowser, onEvent: event => events.push(event) });
    assert.deepEqual(
      events.map(event => (event.type === 'browser' ? event.live_url : event.step.index)),
      ['https://live.example/1', 0, 1],
    );
  });
});

test('a run that ends in a failing status resolves with its result', async () => {
  await withServer(scriptedRun('needs_login'), async fb => {
    const result = await fb.run('Read my orders', ownBrowser);
    assert.equal(result.status, 'needs_login');
    assert.equal(result.error, 'the site asked for a password');
  });
});

test('a missing key rejects with the configuration error code', async () => {
  const server = scriptedRun('echo');
  await withServer({ ...server, env: { ...server.env, BROWSER_USE_API_KEY: '' } }, async fb => {
    await assert.rejects(fb.run('Add the kettle to the cart'), (error: unknown) => {
      assert.ok(error instanceof RpcError);
      assert.equal(error.code, CONFIGURATION);
      assert.match(error.message, /BROWSER_USE_API_KEY/);
      return true;
    });
  });
});

test('a bad option rejects with the invalid params error code, and the next run goes ahead', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    await assert.rejects(fb.run('Add the kettle to the cart', { limits: { maxSteps: 0 } }), (error: unknown) => {
      assert.ok(error instanceof RpcError);
      assert.equal(error.code, INVALID_PARAMS);
      assert.match(error.message, /max_steps/);
      return true;
    });
    assert.equal((await fb.run('Add the kettle to the cart', ownBrowser)).status, 'complete');
  });
});

test('a second run while one is active rejects with the busy error, and hears nothing of the first', async () => {
  const fb = await Fastbrowse.start(scriptedRun('never_ends'));
  const { promise: started, resolve: onEvent } = withResolvers();
  const firstEnded = assert.rejects(fb.run('Wait for the sale to open', { ...ownBrowser, onEvent }), FastbrowseError);
  await started;

  const heardBySecond: unknown[] = [];
  const second = fb.run('Add the kettle to the cart', { ...ownBrowser, onEvent: event => heardBySecond.push(event) });
  await assert.rejects(second, (error: unknown) => {
    assert.ok(error instanceof RpcError);
    assert.equal(error.code, BUSY);
    return true;
  });
  assert.deepEqual(heardBySecond, []);

  await fb.close();
  await firstEnded;
});

test('a server that exits mid-run rejects the run with its exit code', async () => {
  const fb = await Fastbrowse.start(scriptedRun('exits'));
  await assert.rejects(fb.run('Add the kettle to the cart', ownBrowser), (error: unknown) => {
    assert.ok(error instanceof ProcessExitedError);
    assert.equal(error.exitCode, 7);
    assert.equal(error.signal, null);
    assert.match(error.message, /exited with code 7/);
    return true;
  });
  // The process is gone, so a later run has the same answer and does not wait on anything.
  await assert.rejects(fb.run('Add the kettle to the cart', ownBrowser), ProcessExitedError);
  await fb.close();
});

test('a server that is killed mid-run rejects the run with the signal', async () => {
  const fb = await Fastbrowse.start(scriptedRun('killed'));
  await assert.rejects(fb.run('Add the kettle to the cart', ownBrowser), (error: unknown) => {
    assert.ok(error instanceof ProcessExitedError);
    assert.equal(error.exitCode, null);
    assert.equal(error.signal, 'SIGKILL');
    assert.match(error.message, /killed by SIGKILL/);
    return true;
  });
  await fb.close();
});
