// What gets published: the built files, reached the way an installed package is.

import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';

const manifest = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));

test('the package name resolves to the built ES module, with its declarations beside it', async () => {
  // A package can import itself by name, and that goes through `exports` as a dependent's import does.
  const built = await import(manifest.name);
  assert.equal(typeof built.Fastbrowse.start, 'function');
  assert.equal(manifest.type, 'module');
  assert.ok(existsSync(new URL(`../${manifest.exports['.'].types}`, import.meta.url)));
});

test('the package has no runtime dependencies', () => {
  assert.equal(manifest.dependencies, undefined);
  assert.equal(manifest.peerDependencies, undefined);
});
