import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { FastbrowseError } from '../src/errors.ts';
import { readExtensionStatus } from '../src/extension.ts';

/** An executable that runs `body` as a shell script. */
function script(body: string): string {
  const path = join(mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-')), 'fastbrowse');
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
  return path;
}

test('extension status runs `extension status --json` and returns what it prints', async () => {
  const status = {
    registered: true,
    installs: [{ path: '/bin/fastbrowse', version: '0.13.0' }],
    hostDirectory: '/run/user/1/fastbrowse-hosts',
    connected: [{ label: 'Work', browser: 'Chrome', browserId: 'b1', version: '150.0' }],
    unreachable: [],
  };
  const binary = script(`[ "$*" = "extension status --json" ] && echo '${JSON.stringify(status)}'`);
  assert.deepEqual(await readExtensionStatus(binary, process.env), status);
});

test('extension status passes the environment the caller gave', async () => {
  const binary = script('echo "{\\"registered\\": false, \\"hostDirectory\\": \\"$FASTBROWSE_EXTENSION_DIR\\"}"');
  const status = await readExtensionStatus(binary, { ...process.env, FASTBROWSE_EXTENSION_DIR: '/somewhere' });
  assert.equal(status.hostDirectory, '/somewhere');
});

test('extension status rejects with the binary\'s own message when it fails', async () => {
  const binary = script('echo "fastbrowse: cannot parse installs.json" >&2; exit 1');
  await assert.rejects(readExtensionStatus(binary, process.env), (error: unknown) => {
    assert.ok(error instanceof FastbrowseError);
    assert.match(error.message, /cannot parse installs.json/);
    return true;
  });
});

test('extension status rejects when the binary prints something that is not a status', async () => {
  const binary = script('echo "usage: fastbrowse extension install"');
  await assert.rejects(readExtensionStatus(binary, process.env), FastbrowseError);
});
