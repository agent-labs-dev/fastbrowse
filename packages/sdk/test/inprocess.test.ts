import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { test } from 'node:test';

import { Fastbrowse, FastbrowseError } from '../src/index.ts';
import { PROTOCOL_VERSION } from '../src/protocol.ts';
import { packageVersion } from './server.ts';

const addon = process.env.FASTBROWSE_TEST_ADDON;

test('inProcess loads the addon and drives no process', {
  skip: !addon && 'FASTBROWSE_TEST_ADDON names no addon',
}, async () => {
  const fb = await Fastbrowse.start({ inProcess: true, addonPath: addon as string });
  assert.equal(fb.fastbrowseVersion, packageVersion);
  await fb.close();
  await assert.rejects(fb.run('Add the kettle to the cart'), /closed/);
});

test('an addon that cannot be loaded is refused with its path', async () => {
  const missing = join(tmpdir(), 'no-such-fastbrowse.node');
  await assert.rejects(Fastbrowse.start({ inProcess: true, addonPath: missing }), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.ok(error.message.includes(missing));
    return true;
  });
});

/** An addon file speaking `protocol`, with `setPdfHelper` recording what it was given unless `legacy`. */
function stubAddon(dir: string, protocol: number, legacy: boolean): string {
  const path = join(dir, legacy ? 'legacy.cjs' : 'current.cjs');
  const helper = legacy ? '' : 'exports.named = []; exports.setPdfHelper = path => exports.named.push(path) === 1;';
  writeFileSync(
    path,
    `exports.Core = class {}; exports.version = () => ${JSON.stringify(packageVersion)}; ` +
      `exports.protocolVersion = () => ${protocol}; ${helper}`,
  );
  return path;
}

async function withDir(body: (dir: string) => Promise<void>): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), 'fastbrowse-addon-'));
  try {
    await body(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

test('the addon is given the binary that reads its PDFs', () =>
  withDir(async dir => {
    const path = stubAddon(dir, PROTOCOL_VERSION, false);
    const binary = join(dir, 'fastbrowse');
    const fb = await Fastbrowse.start({ inProcess: true, addonPath: path, env: { FASTBROWSE_BINARY: binary } });
    await fb.close();
    assert.deepEqual(createRequire(import.meta.url)(path).named, [binary]);
  }));

test('an explicit binary path also selects the in-process PDF reader', () =>
  withDir(async dir => {
    const path = stubAddon(dir, PROTOCOL_VERSION, false);
    const binary = join(dir, 'chosen-fastbrowse');
    const fb = await Fastbrowse.start({
      inProcess: true,
      addonPath: path,
      binaryPath: binary,
      env: { FASTBROWSE_BINARY: join(dir, 'other-fastbrowse') },
    });
    await fb.close();
    assert.deepEqual(createRequire(import.meta.url)(path).named, [binary]);
  }));

test('an addon of another protocol is refused for its protocol, not for lacking a PDF helper', () =>
  withDir(async dir => {
    const path = stubAddon(dir, PROTOCOL_VERSION - 1, true);
    const env = { FASTBROWSE_BINARY: join(dir, 'fastbrowse') };
    await assert.rejects(Fastbrowse.start({ inProcess: true, addonPath: path, env }), /speaks protocol version/);
  }));
