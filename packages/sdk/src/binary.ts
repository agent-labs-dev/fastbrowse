import { FastbrowseError } from './errors.ts';

/** The executable to start: the caller's own path first, then the one the environment names. */
export function resolveBinary(binaryPath: string | undefined, env: NodeJS.ProcessEnv): string {
  const path = binaryPath ?? env.FASTBROWSE_BINARY;
  if (!path) {
    throw new FastbrowseError('no fastbrowse binary to start: pass binaryPath or set FASTBROWSE_BINARY');
  }
  return path;
}
