// A run the caller can stop with an `AbortSignal`.

import { AbortError, RpcError } from './errors.ts';
import { ErrorCode } from './protocol.ts';

/**
 * Make the `run` request with `send`, and call `cancel` if `signal` aborts while it is waiting on its reply.
 *
 * The server answers a cancelled run with the `cancelled` error once the run's browser has closed, and that
 * reply is what rejects with `AbortError`. A run that finished before the cancellation reached it resolves
 * with its result.
 */
export async function abortable<Result>(
  signal: AbortSignal | undefined,
  send: () => Promise<Result>,
  cancel: () => void,
): Promise<Result> {
  if (!signal) return send();
  if (signal.aborted) throw new AbortError(signal.reason);
  signal.addEventListener('abort', cancel, { once: true });
  try {
    return await send();
  } catch (error) {
    // `shutdown` cancels a run too, and that one the caller did not abort.
    if (signal.aborted && error instanceof RpcError && error.code === ErrorCode.CANCELLED)
      throw new AbortError(signal.reason);
    throw error;
  } finally {
    signal.removeEventListener('abort', cancel);
  }
}
