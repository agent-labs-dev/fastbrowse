import { type ChildProcess, spawn } from 'node:child_process';
import type { Writable } from 'node:stream';

import { resolveBinary } from './binary.ts';
import { FastbrowseError } from './errors.ts';
import { Connection } from './rpc.ts';

/** The wire format this SDK speaks. It is the server's `PROTOCOL_VERSION`, and the handshake compares the two. */
const PROTOCOL_VERSION = 1;

// A run that is cancelled by `shutdown` closes its browser before the process exits, and Chrome takes its time.
const DEFAULT_GRACE_PERIOD_MS = 10_000;

export interface StartOptions {
  /** The fastbrowse executable to run. Without it, the `FASTBROWSE_BINARY` environment variable names one. */
  binaryPath?: string;
  /** Environment variables for the fastbrowse process, on top of the ones this process has. */
  env?: Record<string, string>;
  /**
   * Where the fastbrowse process's stderr goes, which is where it logs. `'inherit'`, the default, is this
   * process's own stderr. A stream receives it and is left open when the process ends.
   */
  stderr?: 'inherit' | 'ignore' | Writable;
}

export interface CloseOptions {
  /** How long the process gets to exit after `shutdown` before it is killed. Ten seconds by default. */
  gracePeriodMs?: number;
}

export class Fastbrowse {
  /** The version of the fastbrowse binary this instance drives. */
  readonly fastbrowseVersion: string;
  readonly #server: Server;
  #closed: Promise<void> | undefined;

  private constructor(server: Server, fastbrowseVersion: string) {
    this.#server = server;
    this.fastbrowseVersion = fastbrowseVersion;
  }

  /** Start a fastbrowse process and check that it speaks this SDK's protocol version. */
  static async start(options: StartOptions = {}): Promise<Fastbrowse> {
    const env = { ...process.env, ...options.env };
    const binary = resolveBinary(options.binaryPath, env);
    const server = new Server(binary, env, options.stderr ?? 'inherit');
    try {
      const reply = (await server.connection.request('initialize', { protocol_version: PROTOCOL_VERSION })) as {
        protocol_version: number;
        fastbrowse_version: string;
      };
      if (reply.protocol_version !== PROTOCOL_VERSION) {
        throw new FastbrowseError(
          `this SDK speaks protocol version ${PROTOCOL_VERSION}, and fastbrowse ${reply.fastbrowse_version} at ` +
            `${binary} speaks protocol version ${reply.protocol_version}: install matching versions of the two`,
        );
      }
      return new Fastbrowse(server, reply.fastbrowse_version);
    } catch (error) {
      await server.kill();
      throw error;
    }
  }

  /**
   * Ask the fastbrowse process to shut down and wait until it has exited. A process still running when the
   * grace period ends is killed. Every call after the first returns the first one's promise.
   */
  close(options: CloseOptions = {}): Promise<void> {
    this.#closed ??= this.#close(options.gracePeriodMs ?? DEFAULT_GRACE_PERIOD_MS);
    return this.#closed;
  }

  async #close(gracePeriodMs: number): Promise<void> {
    // The reply says nothing the exit does not, and a process that died earlier answers with a rejection.
    this.#server.connection.request('shutdown').catch(() => {});
    const overdue = setTimeout(() => void this.#server.kill(), gracePeriodMs);
    await this.#server.exited;
    clearTimeout(overdue);
  }
}

/** The fastbrowse process and the connection over its stdin and stdout. */
class Server {
  readonly connection: Connection;
  /** Resolves once the process is gone and its output has been read to the end. Never rejects. */
  readonly exited: Promise<void>;
  readonly #process: ChildProcess;

  constructor(binary: string, env: NodeJS.ProcessEnv, stderr: 'inherit' | 'ignore' | Writable) {
    const child = spawn(binary, ['serve', '--stdio'], {
      env,
      // `spawn` takes a stream only when a file descriptor backs it, so any other stream is fed from a pipe.
      stdio: ['pipe', 'pipe', typeof stderr === 'string' ? stderr : 'pipe'],
      windowsHide: true,
    });
    if (typeof stderr !== 'string') child.stderr!.pipe(stderr, { end: false });
    this.#process = child;
    this.connection = new Connection(child.stdout!, child.stdin!);
    this.exited = new Promise(resolve => {
      // A binary that cannot be started gets `error` and no `close`.
      child.once('error', error => {
        this.connection.end(new FastbrowseError(`could not start ${binary}: ${error.message}`, { cause: error }));
        resolve();
      });
      child.once('close', (code, signal) => {
        const how = signal ? `was killed by ${signal}` : `exited with code ${code}`;
        this.connection.end(new FastbrowseError(`the fastbrowse process ${how}`));
        resolve();
      });
    });
  }

  async kill(): Promise<void> {
    this.#process.kill('SIGKILL');
    await this.exited;
  }
}
