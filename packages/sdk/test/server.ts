// The server the SDK is tested against: `fastbrowse serve --stdio` from the repository's uv environment, or the
// binary FASTBROWSE_TEST_RUST_BINARY names. Both carry the altered servers behind FASTBROWSE_TEST_SERVER.

import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { delimiter, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { StartOptions } from '../src/index.ts';

const repository = fileURLToPath(new URL('../../../', import.meta.url));
const scripts = process.platform === 'win32' ? 'Scripts' : 'bin';
const suffix = process.platform === 'win32' ? '.exe' : '';

const rust = process.env.FASTBROWSE_TEST_RUST_BINARY;
export const serverBinary = rust ?? join(repository, '.venv', scripts, `fastbrowse${suffix}`);

const pyproject = readFileSync(join(repository, 'pyproject.toml'), 'utf8');
/** The version every server under test reports. */
export const packageVersion = /^version = "(.+)"$/m.exec(pyproject)?.[1];

export interface Launcher {
  /** What `binaryPath` takes. */
  path: string;
  /** Whether the process the SDK started through it is still there. */
  running(): boolean;
}

/**
 * An executable that runs `command` with the arguments it is given. `binaryPath` takes an executable and no
 * arguments, so a server that needs an interpreter and a script gets this around it.
 */
export function launcher(...command: string[]): Launcher {
  const directory = mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-'));
  const path = join(directory, 'fastbrowse');
  const pidFile = join(directory, 'pid');
  const quoted = command.map(word => `'${word}'`).join(' ');
  // `exec` keeps the pid, so the process the SDK started is the server and the pid written here is its own.
  writeFileSync(path, `#!/bin/sh\necho $$ > '${pidFile}'\nexec ${quoted} "$@"\n`);
  chmodSync(path, 0o755);
  return {
    path,
    running() {
      try {
        // Signal 0 delivers nothing and fails when there is no such process.
        process.kill(Number(readFileSync(pidFile, 'utf8')), 0);
        return true;
      } catch {
        return false;
      }
    },
  };
}

/** One of the altered servers FASTBROWSE_TEST_SERVER names: the real server with one thing about it changed. */
export function scriptedServer(name: string, ...args: string[]): Launcher {
  const run = args.length > 0 ? [`FASTBROWSE_TEST_RUN=${args[0]}`] : [];
  return launcher('/usr/bin/env', `FASTBROWSE_TEST_SERVER=${name}`, ...run, serverBinary);
}

/**
 * The addon FASTBROWSE_TEST_ADDON names. With it, every test that starts the server through `scriptedRun` runs on
 * the addon in this process instead, with the stand-in runs the addon carries as the binary does.
 */
const addon = process.env.FASTBROWSE_TEST_ADDON;

/** Set as `skip` on a test of the fastbrowse process itself, which an in-process run has no counterpart of. */
export const processOnly = addon ? 'a test of the fastbrowse process, and this run of the suite is in-process' : false;

/**
 * What `Fastbrowse.start` takes to get the real server with `run`, one of the stand-in runs the server carries
 * behind FASTBROWSE_TEST_SERVER=scripted_run, in place of the agent.
 */
export function scriptedRun(run: string): StartOptions & { env: Record<string, string> } {
  // `--record` is refused without an ffmpeg to encode with, and the machine running the tests may have none.
  const tools = mkdtempSync(join(tmpdir(), 'fastbrowse-sdk-'));
  writeFileSync(join(tools, 'ffmpeg'), '#!/bin/sh\n');
  chmodSync(join(tools, 'ffmpeg'), 0o755);
  // The server refuses a run it has no model keys for, whatever would have run it.
  const keys = { OPENROUTER_API_KEY: 'unused', AI_GATEWAY_API_KEY: 'unused', BROWSER_USE_API_KEY: 'unused' };
  if (addon) {
    // The core looks programs up on this process's PATH, since in-process `env` reaches only the settings.
    process.env.PATH = `${tools}${delimiter}${process.env.PATH}`;
    return {
      inProcess: true,
      addonPath: addon,
      env: { ...keys, FASTBROWSE_TEST_SERVER: 'scripted_run', FASTBROWSE_TEST_RUN: run },
    };
  }
  return {
    binaryPath: scriptedServer('scripted_run', run).path,
    env: { PATH: `${tools}${delimiter}${process.env.PATH}`, ...keys },
  };
}
