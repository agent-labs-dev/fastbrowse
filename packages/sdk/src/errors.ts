/** Every error this package throws, so one `instanceof` tells its failures from the caller's own. */
export class FastbrowseError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

/** The fastbrowse process is gone, so what was waiting on it, or is asked of it later, has no answer. */
export class ProcessExitedError extends FastbrowseError {
  /** The code the process exited with, or null when a signal ended it. */
  readonly exitCode: number | null;
  /** The signal that ended the process, or null when it exited by itself. */
  readonly signal: NodeJS.Signals | null;

  constructor(exitCode: number | null, signal: NodeJS.Signals | null) {
    super(`the fastbrowse process ${signal ? `was killed by ${signal}` : `exited with code ${exitCode}`}`);
    this.exitCode = exitCode;
    this.signal = signal;
  }
}

/**
 * The caller's `AbortSignal` stopped the run. `cause` is the signal's reason, which for `AbortSignal.timeout`
 * is a `TimeoutError`.
 */
export class AbortError extends FastbrowseError {
  constructor(reason: unknown) {
    super('the run was aborted', { cause: reason });
  }
}

/** What a thrown value says. A callback is the caller's code, and it can throw anything. */
export function messageOf(thrown: unknown): string {
  return thrown instanceof Error ? thrown.message : String(thrown);
}

/** The server's error reply to a request. `code` is one of the protocol's error codes. */
export class RpcError extends FastbrowseError {
  readonly code: number;

  constructor(code: number, message: string) {
    super(message);
    this.code = code;
  }
}
