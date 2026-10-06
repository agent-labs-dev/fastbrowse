// JSON-RPC 2.0 over a pair of byte streams, one message per line. Both sides may call the other.

import type { Readable, Writable } from 'node:stream';
import { StringDecoder } from 'node:string_decoder';

import { FastbrowseError, RpcError } from './errors.ts';

type Id = number | string;
type Params = Record<string, unknown>;

interface Pending {
  resolve(result: unknown): void;
  reject(error: Error): void;
}

/** Answers a request the server makes. What it returns is the result, and what it throws is the error reply. */
export type RequestHandler = (params: Params) => unknown;
export type NotificationHandler = (params: Params) => void;

// JSON-RPC's own codes for a request this side cannot serve.
const METHOD_NOT_FOUND = -32601;
const INTERNAL_ERROR = -32603;

export class Connection {
  readonly #output: Writable;
  readonly #pending = new Map<Id, Pending>();
  readonly #requests = new Map<string, RequestHandler>();
  readonly #notifications = new Map<string, NotificationHandler>();
  #nextId = 1;
  #ended: Error | undefined;

  constructor(input: Readable, output: Writable) {
    this.#output = output;
    // A character can straddle two chunks, and decoding each chunk alone would break it.
    const decoder = new StringDecoder('utf8');
    let partial = '';
    input.on('data', (chunk: Buffer) => {
      const lines = (partial + decoder.write(chunk)).split('\n');
      partial = lines.pop() ?? '';
      for (const line of lines) this.#receive(line);
    });
    // A write to a server that has gone fails here. The request it carried is rejected by `end`, which the
    // owner calls when the process exits, so the error has nothing more to say.
    output.on('error', () => {});
  }

  /** Call a method and wait for its reply. Rejects with `RpcError` when the server answers with an error. */
  request(method: string, params: Params = {}): Promise<unknown> {
    if (this.#ended) return Promise.reject(this.#ended);
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#send({ id, method, params });
    });
  }

  notify(method: string, params: Params = {}): void {
    if (!this.#ended) this.#send({ method, params });
  }

  /** Serve a method the server calls on this side. */
  onRequest(method: string, handler: RequestHandler): void {
    this.#requests.set(method, handler);
  }

  onNotification(method: string, handler: NotificationHandler): void {
    this.#notifications.set(method, handler);
  }

  /** The other side is gone. Every request still waiting rejects with `reason`, and so does any made later. */
  end(reason: Error): void {
    if (this.#ended) return;
    this.#ended = reason;
    for (const pending of this.#pending.values()) pending.reject(reason);
    this.#pending.clear();
  }

  #send(message: Record<string, unknown>): void {
    this.#output.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
  }

  #receive(line: string): void {
    let message: Record<string, unknown>;
    try {
      message = JSON.parse(line);
    } catch {
      // The server keeps everything but messages off this stream, so a line that is not one means the binary
      // is not a fastbrowse server at all. Waiting on it would hang every request.
      this.end(new FastbrowseError(`fastbrowse sent a line that is not JSON: ${line.slice(0, 200)}`));
      return;
    }
    if (typeof message.method === 'string') {
      void this.#serve(message.method, (message.params ?? {}) as Params, message.id as Id | undefined);
      return;
    }
    const pending = this.#pending.get(message.id as Id);
    if (!pending) return;
    this.#pending.delete(message.id as Id);
    if (message.error) {
      const { code, message: text } = message.error as { code: number; message: string };
      pending.reject(new RpcError(code, text));
    } else {
      pending.resolve(message.result);
    }
  }

  async #serve(method: string, params: Params, id: Id | undefined): Promise<void> {
    if (id === undefined) {
      this.#notifications.get(method)?.(params);
      return;
    }
    const handler = this.#requests.get(method);
    if (!handler) {
      this.#send({ id, error: { code: METHOD_NOT_FOUND, message: `unknown method '${method}'` } });
      return;
    }
    try {
      this.#send({ id, result: (await handler(params)) ?? null });
    } catch (error) {
      this.#send({
        id,
        error: { code: INTERNAL_ERROR, message: error instanceof Error ? error.message : String(error) },
      });
    }
  }
}
