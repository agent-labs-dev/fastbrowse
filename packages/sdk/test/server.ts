// The server the SDK is tested against: `fastbrowse serve --stdio` from the repository's uv environment.

import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repository = fileURLToPath(new URL('../../../', import.meta.url));
const scripts = process.platform === 'win32' ? 'Scripts' : 'bin';
const suffix = process.platform === 'win32' ? '.exe' : '';

export const serverBinary = join(repository, '.venv', scripts, `fastbrowse${suffix}`);
const python = join(repository, '.venv', scripts, `python${suffix}`);

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

/** One of the scripts in `servers/`. Each is the real server with one thing about it changed. */
export function scriptedServer(name: string): Launcher {
  return launcher(python, fileURLToPath(new URL(`./servers/${name}.py`, import.meta.url)));
}
