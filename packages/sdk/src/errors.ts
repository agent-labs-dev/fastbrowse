/** Every error this package throws, so one `instanceof` tells its failures from the caller's own. */
export class FastbrowseError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

/** The server's error reply to a request. `code` is one of the protocol's error codes. */
export class RpcError extends FastbrowseError {
  readonly code: number;

  constructor(code: number, message: string) {
    super(message);
    this.code = code;
  }
}
