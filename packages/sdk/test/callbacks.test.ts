import assert from 'node:assert/strict';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { Fastbrowse, type RunOptions, type SecretRef } from '../src/index.ts';
import { scriptedRun } from './server.ts';

// A browser the caller already runs, so no run is refused for a missing Chrome. Nothing connects to it.
const ownBrowser = { cdpUrl: 'ws://127.0.0.1:9222/devtools/browser/none' };

async function withRun<T>(run: string, use: (fb: Fastbrowse) => Promise<T>): Promise<T> {
  const fb = await Fastbrowse.start(scriptedRun(run));
  try {
    return await use(fb);
  } finally {
    await fb.close();
  }
}

const shop = 'https://shop.example';
const refs: SecretRef[] = [{ name: 'SHOP_TOTP', origins: [shop] }];

/** A run that types `SHOP_TOTP` twice on `origin`, and resolves with the two values the server was given. */
function typeTwice(fb: Fastbrowse, secrets: RunOptions['secrets'], origin = shop) {
  return fb.run('Sign in', { ...ownBrowser, inputs: { name: 'SHOP_TOTP', origin }, secrets: secrets! });
}

test('secrets.resolve is called with the name and origin, and its value reaches the server', async () => {
  await withRun('types_secrets', async fb => {
    const asked: [string, string][] = [];
    const result = await typeTwice(fb, {
      refs,
      resolve: async (name, origin) => {
        asked.push([name, origin]);
        return '424242';
      },
    });
    assert.equal(result.status, 'complete');
    assert.deepEqual(result.data, ['424242', '424242']);
    assert.deepEqual(asked, [
      ['SHOP_TOTP', shop],
      ['SHOP_TOTP', shop],
    ]);
  });
});

test('secrets.resolve is called each time a value is typed, so a one-time code is fresh', async () => {
  await withRun('types_secrets', async fb => {
    let code = 100;
    const result = await typeTwice(fb, { refs, resolve: () => String(code++) });
    assert.deepEqual(result.data, ['100', '101']);
  });
});

test('a secret the resolver does not hold is null to the server', async () => {
  await withRun('types_secrets', async fb => {
    const result = await typeTwice(fb, { refs, resolve: () => undefined });
    assert.deepEqual(result.data, [null, null]);
  });
});

test('an origin a ref does not cover never reaches secrets.resolve', async () => {
  await withRun('types_secrets', async fb => {
    let asked = 0;
    const result = await typeTwice(fb, { refs, resolve: () => String(asked++) }, 'https://evil.example');
    assert.deepEqual(result.data, [null, null]);
    assert.equal(asked, 0);
  });
});

test('until is called with the address the run ended on, and true lets the run complete', async () => {
  await withRun('ends_on_an_order', async fb => {
    const asked: string[] = [];
    const result = await fb.run('Place the order', {
      ...ownBrowser,
      until: url => {
        asked.push(url);
        return true;
      },
    });
    assert.deepEqual(asked, ['https://shop.example/orders/17']);
    assert.equal(result.status, 'complete');
  });
});

test('until returning false keeps the run from complete', async () => {
  await withRun('ends_on_an_order', async fb => {
    const result = await fb.run('Place the order', { ...ownBrowser, until: async url => url.endsWith('/cart') });
    assert.equal(result.status, 'unverified');
  });
});

for (const answer of ['yes', 1, undefined, null]) {
  test(`until returning ${JSON.stringify(answer)} keeps the run from complete, as anything but true does`, async () => {
    await withRun('ends_on_an_order', async fb => {
      // What plain JavaScript can return, and a check written as `url => url.match(...)` does.
      const until = (() => answer) as unknown as () => boolean;
      assert.equal((await fb.run('Place the order', { ...ownBrowser, until })).status, 'unverified');
    });
  });
}

test('onFrame receives each frame as the bytes the run sent', async () => {
  await withRun('two_frames', async fb => {
    const frames: Uint8Array[] = [];
    const result = await fb.run('Watch the page', { ...ownBrowser, onFrame: frame => void frames.push(frame) });
    assert.equal(result.data, 2);
    assert.deepEqual(frames, [
      Uint8Array.from({ length: 256 }, (_, byte) => byte),
      Uint8Array.from([0xff, 0xd8, ...Buffer.from(' second')]),
    ]);
  });
});

test('without onFrame the run is not filmed', async () => {
  await withRun('two_frames', async fb => {
    assert.equal((await fb.run('Watch the page', ownBrowser)).data, 0);
  });
});

/** The params of the `run` request these options make, as the server read them off the wire. */
async function sent(fb: Fastbrowse, options: RunOptions): Promise<Record<string, unknown>> {
  const result = await fb.run('Sign in', { ...ownBrowser, ...options });
  return (result.data as { params: Record<string, unknown> }).params;
}

test('the run request names a callback only when the caller passes one, and carries the refs alone', async () => {
  await withRun('echo', async fb => {
    const bare = await sent(fb, {});
    for (const field of ['secrets', 'frames', 'until']) assert.equal(field in bare, false, field);

    const held = await sent(fb, {
      secrets: { refs, resolve: () => 'hunter2' },
      until: () => true,
      onFrame: () => {},
    });
    assert.deepEqual(held.secrets, [{ name: 'SHOP_TOTP', origins: [shop] }]);
    assert.equal(held.frames, true);
    assert.equal(held.until, true);
  });
});

const value = 'correct horse battery staple';
const refusals: [string, () => Promise<string>][] = [
  [
    'throws',
    () => {
      throw new Error(`the vault refused ${value}`);
    },
  ],
  ['rejects', () => Promise.reject(new Error(`the vault refused ${value}`))],
  ['rejects with something that is not an error', () => Promise.reject(value)],
];

for (const [how, resolve] of refusals) {
  test(`a secrets.resolve that ${how} ends the run as an error that leaves out what was thrown`, async () => {
    await withRun('types_secrets', async fb => {
      const result = await typeTwice(fb, { refs, resolve });
      assert.equal(result.status, 'error');
      assert.equal(result.error, "secrets/resolve: the client could not resolve the secret 'SHOP_TOTP'");
      assert.doesNotMatch(JSON.stringify(result), /horse/);
      // The server is done with that run, and takes the next one.
      assert.deepEqual((await typeTwice(fb, { refs, resolve: () => '7' })).data, ['7', '7']);
    });
  });
}

test('an until that throws ends the run as an error with its message', async () => {
  await withRun('ends_on_an_order', async fb => {
    const result = await fb.run('Place the order', {
      ...ownBrowser,
      until: () => {
        throw new Error('the order service is down');
      },
    });
    assert.equal(result.status, 'error');
    assert.equal(result.error, 'run/until: the order service is down');
  });
});

test('an until that rejects ends the run as an error with its message', async () => {
  await withRun('ends_on_an_order', async fb => {
    const until = () => Promise.reject(new Error('the order service is down'));
    const result = await fb.run('Place the order', { ...ownBrowser, until });
    assert.equal(result.status, 'error');
    assert.equal(result.error, 'run/until: the order service is down');
  });
});

test('an onFrame that throws ends the run as an error, and hears no more of it', async () => {
  await withRun('two_frames', async fb => {
    let frames = 0;
    const events: unknown[] = [];
    const result = await fb.run('Watch the page', {
      ...ownBrowser,
      onEvent: event => events.push(event),
      onFrame: () => {
        frames += 1;
        throw new Error('the viewer went away');
      },
    });
    assert.equal(result.status, 'error');
    assert.equal(result.error, 'run/frame: the viewer went away');
    assert.equal(frames, 1);
    assert.deepEqual(events, []);
  });
});

test('an onEvent that throws ends the run as an error', async () => {
  await withRun('three_events', async fb => {
    let events = 0;
    const result = await fb.run('Add the kettle to the cart', {
      ...ownBrowser,
      onEvent: () => {
        events += 1;
        throw new Error('the progress bar broke');
      },
    });
    assert.equal(result.status, 'error');
    assert.equal(result.error, 'run/event: the progress bar broke');
    assert.equal(events, 1);
  });
});

test('an onEvent that rejects stops a run that would have gone on, and its browser closes', async () => {
  await withRun('until_cancelled', async fb => {
    const marker = join(mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-')), 'closed');
    const result = await fb.run('Wait for the sale to open', {
      ...ownBrowser,
      inputs: { closed: marker },
      onEvent: () => Promise.reject(new Error('the progress bar broke')),
    });
    assert.equal(result.status, 'error');
    assert.equal(result.error, 'run/event: the progress bar broke');
    assert.ok(existsSync(marker));
    assert.equal((await fb.run('Count the runs', ownBrowser)).status, 'complete');
  });
});
