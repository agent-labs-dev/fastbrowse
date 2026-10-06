import assert from 'node:assert/strict';
import { test } from 'node:test';

import { Fastbrowse } from '../src/index.ts';
import { launcher, scriptedServer, serverBinary } from './server.ts';

test('close resolves once the server process has exited', async () => {
  const server = launcher(serverBinary);
  const fb = await Fastbrowse.start({ binaryPath: server.path });
  assert.equal(server.running(), true);
  await fb.close();
  assert.equal(server.running(), false);
});

test('close kills a server that is still running when the grace period ends', async () => {
  const server = scriptedServer('outlives_shutdown');
  const fb = await Fastbrowse.start({ binaryPath: server.path });
  await fb.close({ gracePeriodMs: 300 });
  assert.equal(server.running(), false);
});

test('close can be called again, and resolves the same way', async () => {
  const fb = await Fastbrowse.start({ binaryPath: serverBinary });
  await Promise.all([fb.close(), fb.close()]);
  await fb.close();
});
