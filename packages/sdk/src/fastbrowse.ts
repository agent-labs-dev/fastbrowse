import { type ChildProcess, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import type { Writable } from 'node:stream';

import { resolveBinary } from './binary.ts';
import { FastbrowseError, ProcessExitedError } from './errors.ts';
import type {
  BrowserEvent,
  InitializeParams,
  InitializeResult,
  RunEvent,
  RunParams,
  RunResult,
  StepEvent,
} from './protocol.ts';
import { Connection } from './rpc.ts';
import { type BrowserOptions, browserParams, type RunOptions, runParams } from './run.ts';

/** The wire format this SDK speaks. It is the server's `PROTOCOL_VERSION`, and the handshake compares the two. */
const PROTOCOL_VERSION = 1;

// A run that is cancelled by `shutdown` closes its browser before the process exits, and Chrome takes its time.
const DEFAULT_GRACE_PERIOD_MS = 10_000;

/** How to start the fastbrowse process, and the browser every run uses unless that run says otherwise. */
export interface StartOptions extends BrowserOptions {
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
  readonly #browser: Partial<RunParams>;
  /** Who hears the events of each run that is waiting on its reply, by run id. */
  readonly #listeners = new Map<string, (event: StepEvent | BrowserEvent) => void>();
  #closed: Promise<void> | undefined;

  private constructor(server: Server, fastbrowseVersion: string, browser: Partial<RunParams>) {
    this.#server = server;
    this.fastbrowseVersion = fastbrowseVersion;
    this.#browser = browser;
    server.connection.onNotification('run/event', params => {
      const { run_id: runId, event } = params as unknown as RunEvent;
      this.#listeners.get(runId)?.(event);
    });
  }

  /** Start a fastbrowse process and check that it speaks this SDK's protocol version. */
  static async start(options: StartOptions = {}): Promise<Fastbrowse> {
    const env = { ...process.env, ...options.env };
    const binary = resolveBinary(options.binaryPath, env);
    const server = new Server(binary, env, options.stderr ?? 'inherit');
    try {
      const hello: InitializeParams = { protocol_version: PROTOCOL_VERSION };
      const reply = (await server.connection.request('initialize', hello)) as InitializeResult;
      if (reply.protocol_version !== PROTOCOL_VERSION) {
        throw new FastbrowseError(
          `this SDK speaks protocol version ${PROTOCOL_VERSION}, and fastbrowse ${reply.fastbrowse_version} at ` +
            `${binary} speaks protocol version ${reply.protocol_version}: install matching versions of the two`,
        );
      }
      return new Fastbrowse(server, reply.fastbrowse_version, browserParams(options));
    } catch (error) {
      await server.kill();
      throw error;
    }
  }

  /**
   * Run a task and resolve with its result, whatever status it ended in: a run that needs a login or got stuck
   * is read from `status`, and is not an exception.
   *
   * Rejects with `RpcError` when the server refuses the request, which it does before a browser opens: for a
   * bad option, for a missing key, and with the busy code while another run on this instance is active. Rejects
   * with `ProcessExitedError` when the fastbrowse process is gone.
   */
  async run(task: string, options: RunOptions = {}): Promise<RunResult> {
    // The server names the run in every event it sends, which is how a run refused as busy hears nothing of
    // the one that is active.
    const runId = randomUUID();
    if (options.onEvent) this.#listeners.set(runId, options.onEvent);
    try {
      const params = runParams(runId, task, this.#browser, options);
      return (await this.#server.connection.request('run', params)) as RunResult;
    } finally {
      this.#listeners.delete(runId);
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
        this.connection.end(new ProcessExitedError(code, signal));
        resolve();
      });
    });
  }

  async kill(): Promise<void> {
    this.#process.kill('SIGKILL');
    await this.exited;
  }
}
