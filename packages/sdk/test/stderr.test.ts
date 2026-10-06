import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { Writable } from 'node:stream';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { Fastbrowse } from '../src/index.ts';
import { scriptedServer } from './server.ts';

const LINE = 'a line the server wrote to stderr';

test("the server's stderr is this process's stderr by default", async () => {
  const fixture = fileURLToPath(new URL('./fixtures/start_and_close.ts', import.meta.url));
  const { stderr } = await promisify(execFile)(process.execPath, [fixture, scriptedServer('stderr_line').path]);
  assert.ok(stderr.includes(LINE), stderr);
});

test("a stream given as stderr receives the server's stderr and is left open", async () => {
  let written = '';
  const sink = new Writable({
    write(chunk, _encoding, done) {
      written += chunk;
      done();
    },
  });
  const fb = await Fastbrowse.start({ binaryPath: scriptedServer('stderr_line').path, stderr: sink });
  await fb.close();
  assert.ok(written.includes(LINE), written);
  // It is the caller's stream, likely shared with their own logs, so the server ending must not end it.
  assert.equal(sink.writableEnded, false);
});

test("stderr: 'ignore' discards the server's stderr", async () => {
  const fixture = fileURLToPath(new URL('./fixtures/start_and_close.ts', import.meta.url));
  const { stderr } = await promisify(execFile)(process.execPath, [
    fixture,
    scriptedServer('stderr_line').path,
    'ignore',
  ]);
  assert.ok(!stderr.includes(LINE), stderr);
});
