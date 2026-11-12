// The agent inside this process: the Rust core as a Node addon (crates/fastbrowse-node), driven with the same `run`
// params and the same result the fastbrowse process exchanges over JSON-RPC.

import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';

import { Callbacks } from './callbacks.ts';
import { abortable } from './abort.ts';
import { platformPackage, resolveBinary } from './binary.ts';
import { FastbrowseError, messageOf, RpcError } from './errors.ts';
import { ErrorCode, type RunParams, type RunResult, type Error as WireError } from './protocol.ts';
import type { RunOptions } from './run.ts';

/** A host function of the addon. It answers `{"ok": value}` or `{"error": message}` as JSON, and never rejects. */
type HostFunction<Argument> = (argument: Argument) => Promise<string>;

interface AddonHost {
  onEvent?: HostFunction<string>;
  onFrame?: HostFunction<Buffer>;
  until?: HostFunction<string>;
  resolveSecret?: HostFunction<string>;
}

interface AddonCancel {
  cancel(): void;
}

/** What `crates/fastbrowse-node` exports. */
export interface Addon {
  Core: new (
    env?: Record<string, string>,
  ) => {
    /** Resolves with `{"result": RunResult}` or `{"error": {"code", "message", "data"?}}` as JSON. */
    runTask(params: string, host?: AddonHost, cancel?: AddonCancel): Promise<string>;
  };
  RunCancel: new () => AddonCancel;
  version(): string;
  protocolVersion(): number;
  /**
   * Read PDFs by running this fastbrowse binary as a helper process. False when a helper was already set.
   */
  setPdfHelper(path: string): boolean;
}

/**
 * The addon to load: the caller's own path first, then the one the environment names, then the `fastbrowse.node`
 * beside the binary in the platform package.
 */
export function loadAddon(addonPath: string | undefined, env: NodeJS.ProcessEnv): Addon {
  const path = addonPath ?? env.FASTBROWSE_ADDON ?? join(dirname(platformPackage()), 'fastbrowse.node');
  try {
    return createRequire(import.meta.url)(path) as Addon;
  } catch (error) {
    throw new FastbrowseError(`could not load the fastbrowse addon at ${path}: ${messageOf(error)}`, {
      cause: error,
    });
  }
}

/**
 * Have `addon` read PDFs with the fastbrowse binary `env` resolves to, once its protocol is known to match. A PDF is
 * read in a process the run can kill, and Node is no program the addon can run again for that: the binary beside it
 * is. Without a binary, the addon still runs and says of each PDF that its text went unread.
 */
export function readPdfsWith(addon: Addon, env: NodeJS.ProcessEnv, binaryPath?: string): void {
  let binary: string;
  try {
    binary = resolveBinary(binaryPath, env);
  } catch {
    return;
  }
  addon.setPdfHelper(binary);
}

/** The JSON a host function answers with. What a callback threw reaches the core as its message alone. */
async function answered(call: () => unknown): Promise<string> {
  try {
    return JSON.stringify({ ok: (await call()) ?? null });
  } catch (error) {
    return JSON.stringify({ error: messageOf(error) });
  }
}

/** Runs on the addon, one at a time as the fastbrowse process runs them, so the two modes answer alike. */
export class InProcess {
  readonly #addon: Addon;
  readonly #core: InstanceType<Addon['Core']>;
  readonly #callbacks = new Callbacks();
  #active: { runId: string; cancel: AddonCancel; done: Promise<unknown> } | undefined;
  #closed = false;

  constructor(addon: Addon, env: Record<string, string>) {
    this.#addon = addon;
    this.#core = new addon.Core(env);
  }

  async run(runId: string, params: RunParams, options: RunOptions): Promise<RunResult> {
    if (this.#closed) throw new FastbrowseError('this fastbrowse instance is closed');
    const cancel = new this.#addon.RunCancel();
    const host: AddonHost = {
      onEvent: event => answered(() => this.#callbacks.event(runId, JSON.parse(event))),
    };
    if (params.frames) host.onFrame = frame => answered(() => this.#callbacks.frame(runId, Uint8Array.from(frame)));
    if (params.until) host.until = url => answered(() => this.#callbacks.until(runId, url));
    if (params.secrets?.length) {
      host.resolveSecret = asked =>
        answered(() => {
          const [name, origin] = JSON.parse(asked) as [string, string];
          return this.#callbacks.resolve(runId, name, origin);
        });
    }
    const send = async (): Promise<RunResult> => {
      if (this.#active) {
        const message = `run '${this.#active.runId}' is still active, and a server runs one at a time`;
        throw new RpcError(ErrorCode.BUSY, message);
      }
      const done = this.#core.runTask(JSON.stringify(params), host, cancel);
      this.#active = { runId, cancel, done };
      let reply: { result?: RunResult; error?: WireError };
      try {
        reply = JSON.parse(await done);
      } finally {
        this.#active = undefined;
      }
      if (reply.error) throw new RpcError(reply.error.code, reply.error.message, reply.error.data ?? undefined);
      return reply.result as RunResult;
    };
    const stop = () => cancel.cancel();
    return this.#callbacks.during(runId, options, stop, () => abortable(options.signal, send, stop));
  }

  /** Stop the run under way, as `shutdown` stops it, and wait until its browser has closed. */
  async close(): Promise<void> {
    this.#closed = true;
    const active = this.#active;
    if (!active) return;
    active.cancel.cancel();
    await active.done.catch(() => {});
  }
}
