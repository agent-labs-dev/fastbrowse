import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, test } from 'node:test';

import { Fastbrowse, FastbrowseError } from '../src/index.ts';
import { launcher, scriptedServer, serverBinary } from './server.ts';

const pyproject = readFileSync(new URL('../../../pyproject.toml', import.meta.url), 'utf8');
const packageVersion = /^version = "(.+)"$/m.exec(pyproject)?.[1];

afterEach(() => {
  delete process.env.FASTBROWSE_BINARY;
  delete process.env.FASTBROWSE_TEST_INHERITED;
  delete process.env.FASTBROWSE_TEST_OVERRIDDEN;
});

test('start with binaryPath resolves once the server has answered initialize', async () => {
  const fb = await Fastbrowse.start({ binaryPath: serverBinary });
  try {
    assert.equal(fb.fastbrowseVersion, packageVersion);
  } finally {
    await fb.close();
  }
});

test('start takes the binary from FASTBROWSE_BINARY when no binaryPath is given', async () => {
  process.env.FASTBROWSE_BINARY = serverBinary;
  const fb = await Fastbrowse.start();
  try {
    assert.equal(fb.fastbrowseVersion, packageVersion);
  } finally {
    await fb.close();
  }
});

test('binaryPath wins over FASTBROWSE_BINARY', async () => {
  process.env.FASTBROWSE_BINARY = join(tmpdir(), 'no-such-fastbrowse');
  const fb = await Fastbrowse.start({ binaryPath: serverBinary });
  await fb.close();
});

test('start rejects when neither binaryPath nor FASTBROWSE_BINARY names a binary', async () => {
  await assert.rejects(Fastbrowse.start(), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.match(error.message, /binaryPath/);
    assert.match(error.message, /FASTBROWSE_BINARY/);
    return true;
  });
});

test('start rejects with the path when the binary cannot be started', async () => {
  const missing = join(tmpdir(), 'no-such-fastbrowse');
  await assert.rejects(Fastbrowse.start({ binaryPath: missing }), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.ok(error.message.includes(missing));
    return true;
  });
});

test('start rejects when the process exits before it answers', async () => {
  const dies = launcher('/bin/sh', '-c', 'exit 3');
  await assert.rejects(Fastbrowse.start({ binaryPath: dies.path }), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.match(error.message, /exited with code 3/);
    return true;
  });
});

test('a binary on another protocol version is refused with both versions named, and stopped', async () => {
  const server = scriptedServer('protocol_2');
  await assert.rejects(Fastbrowse.start({ binaryPath: server.path }), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.match(error.message, /protocol version 1\b/);
    assert.match(error.message, /protocol version 2\b/);
    return true;
  });
  assert.equal(server.running(), false);
});

test('env reaches the server merged over the inherited environment', async () => {
  const file = join(mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-')), 'environment.json');
  process.env.FASTBROWSE_TEST_INHERITED = 'from the parent';
  process.env.FASTBROWSE_TEST_OVERRIDDEN = 'from the parent';
  const fb = await Fastbrowse.start({
    binaryPath: scriptedServer('environment').path,
    env: {
      FASTBROWSE_TEST_ENVIRONMENT_FILE: file,
      FASTBROWSE_TEST_GIVEN: 'from start',
      FASTBROWSE_TEST_OVERRIDDEN: 'from start',
    },
  });
  await fb.close();
  const seen = JSON.parse(readFileSync(file, 'utf8'));
  assert.equal(seen.FASTBROWSE_TEST_INHERITED, 'from the parent');
  assert.equal(seen.FASTBROWSE_TEST_GIVEN, 'from start');
  assert.equal(seen.FASTBROWSE_TEST_OVERRIDDEN, 'from start');
});
