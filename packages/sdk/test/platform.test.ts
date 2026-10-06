// Where the binary comes from when the caller names none: the platform package installed beside the SDK.

import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { chmodSync, copyFileSync, cpSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { type Host, resolveBinary } from '../src/binary.ts';
import { FastbrowseError } from '../src/index.ts';
import { launcher, serverBinary } from './server.ts';

const sdk = fileURLToPath(new URL('../', import.meta.url));
const pyproject = readFileSync(new URL('../../../pyproject.toml', import.meta.url), 'utf8');
const packageVersion = /^version = "(.+)"$/m.exec(pyproject)?.[1];

// Starts with the options given as JSON, prints the version the server reported, and closes.
const START = `
import { Fastbrowse } from 'fastbrowse';
const fb = await Fastbrowse.start(JSON.parse(process.argv[2] ?? '{}'));
console.log(fb.fastbrowseVersion);
await fb.close();
`;

/**
 * A project with the built SDK installed, and this machine's platform package beside it holding `binary`.
 * Returns what running a script that starts the SDK in that project printed.
 */
function project(binary: string): (options?: object, env?: Record<string, string>) => Promise<string> {
  const root = mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-'));
  const installed = join(root, 'node_modules', 'fastbrowse');
  mkdirSync(installed, { recursive: true });
  copyFileSync(join(sdk, 'package.json'), join(installed, 'package.json'));
  cpSync(join(sdk, 'dist'), join(installed, 'dist'), { recursive: true });

  const target = `${process.platform}-${process.arch}`;
  const platformPackage = join(root, 'node_modules', '@fastbrowse', target);
  mkdirSync(join(platformPackage, 'fastbrowse'), { recursive: true });
  writeFileSync(join(platformPackage, 'package.json'), JSON.stringify({ name: `@fastbrowse/${target}` }));
  copyFileSync(binary, join(platformPackage, 'fastbrowse', 'fastbrowse'));
  chmodSync(join(platformPackage, 'fastbrowse', 'fastbrowse'), 0o755);

  writeFileSync(join(root, 'start.mjs'), START);
  return async (options = {}, env = {}) => {
    const { FASTBROWSE_BINARY: _, ...inherited } = process.env;
    const { stdout } = await promisify(execFile)(process.execPath, ['start.mjs', JSON.stringify(options)], {
      cwd: root,
      env: { ...inherited, ...env },
    });
    return stdout.trim();
  };
}

test('start with no binary named runs the one in the installed platform package', async () => {
  const start = project(launcher(serverBinary).path);
  assert.equal(await start(), packageVersion);
});

test('binaryPath wins over the platform package', async () => {
  const start = project(launcher('/bin/sh', '-c', 'exit 3').path);
  assert.equal(await start({ binaryPath: serverBinary }), packageVersion);
});

test('FASTBROWSE_BINARY wins over the platform package', async () => {
  const start = project(launcher('/bin/sh', '-c', 'exit 3').path);
  assert.equal(await start({}, { FASTBROWSE_BINARY: serverBinary }), packageVersion);
});

/** A machine with no fastbrowse platform package installed. */
function machine(platform: string, arch: string, libc: 'glibc' | 'musl' = 'glibc'): Host {
  return {
    platform,
    arch,
    musl: () => libc === 'musl',
    resolve(specifier) {
      throw new Error(`Cannot find module '${specifier}'`);
    },
  };
}

function refusal(on: Host): string {
  try {
    resolveBinary(undefined, {}, on);
  } catch (error) {
    assert.ok(error instanceof FastbrowseError);
    return error.message;
  }
  assert.fail('a binary was resolved');
}

test('a platform fastbrowse has no binary for is refused by platform and arch', () => {
  const message = refusal(machine('freebsd', 'riscv64'));
  assert.match(message, /\bfreebsd\b/);
  assert.match(message, /\briscv64\b/);
  assert.match(message, /binaryPath/);
  assert.match(message, /FASTBROWSE_BINARY/);
});

test('an arch with no binary on a platform that has others is refused the same way', () => {
  const message = refusal(machine('win32', 'arm64'));
  assert.match(message, /\bwin32 arm64\b/);
});

test('on musl the refusal names the platform and arch and says musl is the cause', () => {
  const message = refusal(machine('linux', 'x64', 'musl'));
  assert.match(message, /\blinux x64\b/);
  assert.match(message, /musl/);
  assert.match(message, /glibc/);
});

test('musl is the cause even when a package manager installed the glibc package', () => {
  const alpine: Host = {
    ...machine('linux', 'x64', 'musl'),
    resolve: () => '/app/node_modules/@fastbrowse/linux-x64/package.json',
  };
  assert.match(refusal(alpine), /musl/);
});

test('a supported platform whose package is not installed is told which package is missing', () => {
  const message = refusal(machine('linux', 'arm64'));
  assert.match(message, /\blinux arm64\b/);
  assert.ok(message.includes('@fastbrowse/linux-arm64'), message);
  assert.doesNotMatch(message, /musl/);
  assert.match(message, /binaryPath/);
  assert.match(message, /FASTBROWSE_BINARY/);
});

test('every platform package this package depends on is one it takes a binary from', () => {
  const manifest = JSON.parse(readFileSync(join(sdk, 'package.json'), 'utf8'));
  for (const name of Object.keys(manifest.optionalDependencies)) {
    const [platform, arch] = name.replace('@fastbrowse/', '').split('-') as [string, string];
    const found = resolveBinary(undefined, {}, { ...machine(platform, arch), resolve: s => join('/app', s) });
    assert.ok(found.startsWith(join('/app', name, 'fastbrowse')), found);
  }
});

test('the binary in a platform package is the executable inside its fastbrowse directory', () => {
  const installed = (platform: string, arch: string): Host => ({
    ...machine(platform, arch),
    resolve: specifier => join('/app/node_modules', specifier),
  });
  assert.equal(
    resolveBinary(undefined, {}, installed('darwin', 'x64')),
    join('/app/node_modules/@fastbrowse/darwin-x64/fastbrowse/fastbrowse'),
  );
  assert.equal(
    resolveBinary(undefined, {}, installed('win32', 'x64')),
    join('/app/node_modules/@fastbrowse/win32-x64/fastbrowse/fastbrowse.exe'),
  );
});
