import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';

import { FastbrowseError } from './errors.ts';

/** The `<platform>-<arch>` pairs a binary is published for, each as the package `@fastbrowse/<pair>`. */
const TARGETS = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64'];

const OWN_BINARY = 'pass binaryPath or set FASTBROWSE_BINARY to run a binary of your own';

/** What resolution asks of the machine it runs on. */
export interface Host {
  platform: string;
  arch: string;
  /** Whether the C library is musl, as on Alpine. Asked on Linux only. */
  musl(): boolean;
  /** The path of a module installed where this package can import it. Throws when there is none. */
  resolve(specifier: string): string;
}

const host: Host = {
  platform: process.platform,
  arch: process.arch,
  musl() {
    // Node's report names the glibc it runs on, and has no such entry on musl. It also lists the network
    // interfaces unless told not to, which is slow on a machine with many. Node 20.13 added the switch, later
    // than the types this package builds against.
    const report = process.report as unknown as {
      excludeNetwork?: boolean;
      getReport(): { header: { glibcVersionRuntime?: string } };
    };
    const listed = report.excludeNetwork;
    report.excludeNetwork = true;
    try {
      return report.getReport().header.glibcVersionRuntime === undefined;
    } finally {
      if (listed !== undefined) report.excludeNetwork = listed;
    }
  },
  resolve: createRequire(import.meta.url).resolve,
};

/**
 * The executable to start: the caller's own path first, then the one the environment names, then the one in
 * the platform package installed beside this package.
 */
export function resolveBinary(binaryPath: string | undefined, env: NodeJS.ProcessEnv, on: Host = host): string {
  const named = binaryPath ?? env.FASTBROWSE_BINARY;
  if (named) return named;

  const target = `${on.platform}-${on.arch}`;
  if (!TARGETS.includes(target)) {
    throw new FastbrowseError(
      `fastbrowse has no binary for ${on.platform} ${on.arch}. It is published for ${TARGETS.join(', ')}. ` +
        `On another platform, ${OWN_BINARY}.`,
    );
  }
  // Asked before the package is looked for: a package manager that does not read `libc` installs the glibc
  // build on musl, where starting it fails with a "no such file" that names a file which is there.
  if (on.platform === 'linux' && on.musl()) {
    throw new FastbrowseError(
      `fastbrowse has no binary for ${on.platform} ${on.arch} with musl, the C library this system uses. ` +
        `Its Linux binaries need glibc. On musl, ${OWN_BINARY}.`,
    );
  }
  const name = `@fastbrowse/${target}`;
  let manifest: string;
  try {
    // A platform package has no `exports`, and its manifest is the one file certain to be at its root.
    manifest = on.resolve(`${name}/package.json`);
  } catch (error) {
    throw new FastbrowseError(
      `${name}, which holds the fastbrowse binary for ${on.platform} ${on.arch}, is not installed. It is an ` +
        'optional dependency of fastbrowse, so install without --omit=optional or --no-optional. ' +
        `Or ${OWN_BINARY}.`,
      { cause: error },
    );
  }
  return join(dirname(manifest), 'fastbrowse', on.platform === 'win32' ? 'fastbrowse.exe' : 'fastbrowse');
}
