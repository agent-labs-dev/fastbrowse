import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { Fastbrowse, FastbrowseError } from '../src/index.ts';
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
