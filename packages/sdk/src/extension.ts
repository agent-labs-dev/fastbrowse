// The browser extension's state, read without starting a run: `fastbrowse extension status --json`.

import { execFile } from 'node:child_process';

import { FastbrowseError, messageOf } from './errors.ts';

/** A fastbrowse registered as the extension's native host. */
export interface ExtensionInstall {
  path: string;
  version: string;
}

/** A browser whose extension is connected. `label` is what `extensionBrowser` takes. */
export interface ConnectedBrowser {
  label: string;
  /** The browser's product, such as `Chrome`. */
  browser: string;
  browserId: string;
  version: string;
}

/** A host that is present but that this process could not read or reach, as one behind a sandbox. */
export interface UnreachableHost {
  /** The browser's label, or null when the record could not be read. */
  label: string | null;
  browser: string | null;
  reason: string;
}

export interface ExtensionStatus {
  /** Whether a fastbrowse is registered as the native host. */
  registered: boolean;
  /** The registered installs; the first is the one the host runs. */
  installs: ExtensionInstall[];
  /** The directory the host records are read from. */
  hostDirectory: string;
  connected: ConnectedBrowser[];
  unreachable: UnreachableHost[];
}

/** Run `binary extension status --json` and parse what it prints. Starts no run and spends nothing. */
export function readExtensionStatus(binary: string, env: NodeJS.ProcessEnv): Promise<ExtensionStatus> {
  return new Promise((resolve, reject) => {
    execFile(
      binary,
      ['extension', 'status', '--json'],
      { env, windowsHide: true, maxBuffer: 16 * 1024 * 1024 },
      (error, stdout, stderr) => {
        if (error) {
          const detail = (stderr || stdout || messageOf(error)).trim();
          const message = `could not read the extension status from ${binary}: ${detail}`;
          reject(new FastbrowseError(message, { cause: error }));
          return;
        }
        try {
          resolve(JSON.parse(stdout) as ExtensionStatus);
        } catch (parseError) {
          const message = `${binary} printed no extension status: ${messageOf(parseError)}`;
          reject(new FastbrowseError(message, { cause: parseError }));
        }
      },
    );
  });
}
