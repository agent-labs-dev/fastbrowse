// What gets published: the built files, reached the way an installed package is.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const manifest = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));

test('the tarball holds a README, which is what the package page on npm shows', () => {
  const root = fileURLToPath(new URL('../', import.meta.url));
  const listed = JSON.parse(execFileSync('npm', ['pack', '--dry-run', '--json'], { cwd: root, encoding: 'utf8' }));
  // A list of one package up to npm 11, and from npm 12 an object with the package under its name.
  const [packed] = Object.values<{ files: { path: string }[] }>(listed);
  const paths = packed?.files.map(file => file.path) ?? [];
  assert.ok(paths.includes('README.md'), paths.join(', '));
  assert.ok(paths.includes('dist/index.js'), paths.join(', '));
});

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

test('the five platform packages are optional dependencies at exactly the version of this package', () => {
  assert.deepEqual(manifest.optionalDependencies, {
    '@fastbrowse/darwin-arm64': manifest.version,
    '@fastbrowse/darwin-x64': manifest.version,
    '@fastbrowse/linux-arm64': manifest.version,
    '@fastbrowse/linux-x64': manifest.version,
    '@fastbrowse/win32-x64': manifest.version,
  });
});

test('nothing runs when the package is installed', () => {
  // pnpm and bun block install scripts unless a project allows them, so a package that needed one would not work.
  for (const script of ['preinstall', 'install', 'postinstall', 'prepare']) {
    assert.equal(manifest.scripts?.[script], undefined, script);
  }
});
