import { type ChildProcess, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import type { Writable } from 'node:stream';

import { abortable } from './abort.ts';
import { Callbacks } from './callbacks.ts';
import { resolveBinary } from './binary.ts';
import { FastbrowseError, ProcessExitedError } from './errors.ts';
import { type OutputSchema, type TypedRunResult, withOutput } from './output.ts';
import {
  type InitializeParams,
  type InitializeResult,
  Method,
  PROTOCOL_VERSION,
  type RunCancelParams,
  type RunParams,
  type RunResult,
} from './protocol.ts';
import { Connection } from './rpc.ts';
import { type BrowserOptions, browserParams, type RunOptions, runParams } from './run.ts';

// Three times the 10 s the server gives Chrome to exit before it kills it: a server killed first orphans that Chrome.
const DEFAULT_GRACE_PERIOD_MS = 30_000;

/** How to start the fastbrowse process, and the browser every run uses unless that run says otherwise. */
export interface StartOptions extends BrowserOptions {
  /**
   * The fastbrowse executable to run. Without it, the `FASTBROWSE_BINARY` environment variable names one, and
   * without that it is the one in the `@fastbrowse/<platform>-<arch>` package installed with this package.
   */
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
  /** How long the process gets to exit after `shutdown` before it is killed. Thirty seconds by default. */
  gracePeriodMs?: number;
}

export class Fastbrowse {
  /** The version of the fastbrowse binary this instance drives. */
  readonly fastbrowseVersion: string;
  readonly #server: Server;
  readonly #browser: Partial<RunParams>;
  readonly #callbacks: Callbacks;
  #closed: Promise<void> | undefined;

  private constructor(server: Server, fastbrowseVersion: string, browser: Partial<RunParams>) {
    this.#server = server;
    this.fastbrowseVersion = fastbrowseVersion;
    this.#browser = browser;
    this.#callbacks = new Callbacks(server.connection);
  }

  /** Start a fastbrowse process and check that it speaks this SDK's protocol version. */
  static async start(options: StartOptions = {}): Promise<Fastbrowse> {
    const env = { ...process.env, ...options.env };
    const binary = resolveBinary(options.binaryPath, env);
    const server = new Server(binary, env, options.stderr ?? 'inherit');
    try {
      const hello: InitializeParams = { protocol_version: PROTOCOL_VERSION };
      const reply = (await server.connection.request(Method.INITIALIZE, hello)) as InitializeResult;
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
   * bad option, for a missing key, and with the busy code while another run on this instance is active. A run
   * that fails after that resolves with status `error`. Rejects with `ProcessExitedError` when the fastbrowse
   * process is gone. Rejects with `AbortError` when `signal` stopped the run.
   *
   * With `output`, the result's `output` is the data as that schema validated it. A schema that cannot be sent
   * rejects before anything is, a keyword the server cannot enforce rejects with its `unsupported_schema` code,
   * and data the schema refuses rejects with `OutputValidationError`, which carries the result.
   */
  async run<Schema extends OutputSchema>(
    task: string,
    options: RunOptions & { output?: Schema } = {},
  ): Promise<TypedRunResult<Schema>> {
    // The server names the run in every event it sends, which is how a run refused as busy hears nothing of
    // the one that is active.
    const runId = randomUUID();
    const params = runParams(runId, task, this.#browser, options);
    const { connection } = this.#server;
    // The reply says only that the request was read, and a process that has gone rejects the run itself.
    const cancel = () =>
      void connection.request(Method.RUN_CANCEL, { run_id: runId } satisfies RunCancelParams).catch(() => {});
    const result = await this.#callbacks.during(runId, options, cancel, () =>
      abortable(options.signal, () => connection.request(Method.RUN, params) as Promise<RunResult>, cancel),
    );
    return withOutput(result, options.output);
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
    this.#server.connection.request(Method.SHUTDOWN).catch(() => {});
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
