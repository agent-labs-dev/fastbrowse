// What the server sends while a run is under way, handed to the callbacks of the run it names.

import { CANCELLED } from './abort.ts';
import { FastbrowseError, messageOf, RpcError } from './errors.ts';
import type { RunEvent, RunFrame, RunResult, RunUntilParams, SecretsResolveParams, ServerMethod } from './protocol.ts';
import type { Connection } from './rpc.ts';
import type { RunOptions } from './run.ts';

/** A run that is waiting on its reply. */
interface Served {
  options: RunOptions;
  cancel: () => void;
  /** What ended the run from this side: the first `onEvent` or `onFrame` that threw. */
  failure?: string;
}

export class Callbacks {
  readonly #runs = new Map<string, Served>();

  constructor(connection: Connection) {
    connection.onNotification('run/event', params => {
      const { run_id: runId, event } = params as unknown as RunEvent;
      this.#tell(runId, 'run/event', options => options.onEvent?.(event));
    });
    connection.onNotification('run/frame', params => {
      const { run_id: runId, frame } = params as unknown as RunFrame;
      this.#tell(runId, 'run/frame', options => options.onFrame?.(Uint8Array.from(Buffer.from(frame, 'base64'))));
    });
    connection.onRequest('run/until', params => {
      const { run_id: runId, url } = params as unknown as RunUntilParams;
      return this.#asked(runId, 'until')(url);
    });
    connection.onRequest('secrets/resolve', async params => {
      const { run_id: runId, name, origin } = params as unknown as SecretsResolveParams;
      const secrets = this.#asked(runId, 'secrets');
      try {
        return await secrets.resolve(name, origin);
      } catch {
        // The server puts this message in the run's result, and what a resolver throws may quote the value.
        throw new FastbrowseError(`the resolver threw for the secret '${name}'`);
      }
    });
  }

  /**
   * Hand the server's calls for run `runId` to `options` until `run` settles, and settle as it does.
   *
   * A request the server makes is answered with an error when its callback throws, and the server ends the
   * run for that. `onEvent` and `onFrame` are notifications with no reply to fail, so one that throws has the
   * run cancelled from here and resolved as the server would have: status `error`, and the text naming the
   * method and the message.
   */
  async during(
    runId: string,
    options: RunOptions,
    cancel: () => void,
    run: () => Promise<RunResult>,
  ): Promise<RunResult> {
    const served: Served = { options, cancel };
    this.#runs.set(runId, served);
    try {
      const result = await run();
      // A run can finish before the cancellation reaches it, and its callback failed all the same.
      return served.failure === undefined ? result : endedBy(served.failure);
    } catch (error) {
      if (served.failure !== undefined && error instanceof RpcError && error.code === CANCELLED) {
        return endedBy(served.failure);
      }
      throw error;
    } finally {
      this.#runs.delete(runId);
    }
  }

  #tell(runId: string, method: ServerMethod, call: (options: RunOptions) => unknown): void {
    const served = this.#runs.get(runId);
    if (!served || served.failure !== undefined) return;
    const fail = (error: unknown) => {
      // An async callback can reject after its run was answered, and then there is no run left to end.
      if (served.failure !== undefined || this.#runs.get(runId) !== served) return;
      served.failure = `${method}: ${messageOf(error)}`;
      served.cancel();
    };
    // A callback that throws is known to have failed before the next line is read, so it is not called again.
    try {
      Promise.resolve(call(served.options)).catch(fail);
    } catch (error) {
      fail(error);
    }
  }

  #asked<Name extends 'until' | 'secrets'>(runId: string, name: Name): NonNullable<RunOptions[Name]> {
    const callback = this.#runs.get(runId)?.options[name];
    if (!callback) throw new FastbrowseError(`run '${runId}' has no ${name} callback waiting`);
    return callback;
  }
}

/** The result of a run that a throwing callback ended, in the shape the server gives one. */
function endedBy(failure: string): RunResult {
  return {
    status: 'error',
    budget: null,
    answer: null,
    data: null,
    evidence: [],
    steps: [],
    cost: { lines: [] },
    artifacts: [],
    citations: [],
    error: failure,
    final_url: null,
    would_fire: [],
    recordings: [],
  };
}
