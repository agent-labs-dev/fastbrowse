// What a caller passes to `run`, and the `run` request it becomes. Options are camelCase, and the wire keeps the
// Python models' snake_case names.

import { type OutputSchema, wireSchema } from './output.ts';
import type { Authorization, BrowserEvent, Limits, LocalChrome, RunParams, SecretRef, StepEvent } from './protocol.ts';

type Camel<Name extends string> = Name extends `${infer Head}_${infer Tail}`
  ? `${Head}${Capitalize<Camel<Tail>>}`
  : Name;

/** A wire model with its field names in camelCase, so its fields follow the Python model without a second list. */
export type CamelKeys<Model> = { [Name in keyof Model as Camel<Name & string>]: Model[Name] };

/** Which browser a run drives. Given to `Fastbrowse.start`, these are the defaults of every run it makes. */
export interface BrowserOptions {
  /** Local Chrome instead of a Browser Use Cloud browser. */
  local?: boolean;
  /** Which local Chrome, and how. A headed window or a kept profile implies `local`. */
  chrome?: CamelKeys<LocalChrome>;
  cloudProfile?: string;
  /** The DevTools WebSocket URL of a browser that is already running. The run neither starts nor stops it. */
  cdpUrl?: string;
  /** The DevTools port of a browser that is already running on this machine. */
  cdpPort?: number;
  /** Drive a window that is already open in the browser `cdpUrl` or `cdpPort` names, and open no tab. */
  attach?: boolean;
  /** Part of the URL or title of the window to attach to. */
  targetMatch?: string;
  /** Bring the run's tab and its browser window to the front, to watch it. A visible window otherwise stays in the background. */
  foreground?: boolean;
  /** The two-letter country a cloud browser browses from. */
  proxyCountry?: string;
  viewport?: [width: number, height: number];
  cloudAllowResizing?: boolean;
}

export interface Attachment {
  name: string;
  mimeType: string;
  content: Uint8Array;
}

/** The secrets a run may type. The server and the models see the refs, and the values stay with `resolve`. */
export interface Secrets {
  refs: SecretRef[];
  /**
   * The value of the secret `name`, about to be typed on `origin`, or nothing when there is none. Called each
   * time a value is typed and never for an origin the ref does not cover, so a one-time code is computed
   * when it is needed. If it throws, the run ends with status `error`, and what it threw is left out of the
   * result.
   */
  resolve: (name: string, origin: string) => MaybePromise<string | null | undefined>;
}

type MaybePromise<Value> = Value | Promise<Value>;

/**
 * Each option as one run may give it. Null takes the default `Fastbrowse.start` was given out of that run:
 * a default `cdpPort` has to go before the run can ask for `local`, and leaving the option out keeps it.
 */
export type PerRun<Options> = { [Name in keyof Options]?: Options[Name] | null };

export interface RunOptions extends PerRun<BrowserOptions> {
  /** The address to start on. Without one, the first address is proposed from the task. */
  start?: string;
  inputs?: Record<string, string>;
  /** Files the run may upload. */
  attachments?: Attachment[];
  limits?: CamelKeys<Limits>;
  authorization?: CamelKeys<Authorization>;
  /**
   * The shape of the data to read: a Zod, ArkType or other Standard Schema that can write itself as JSON
   * Schema, or a JSON Schema object. The first also validates what comes back, and types `output`.
   */
  output?: OutputSchema;
  /** A directory that keeps the files the run downloads. */
  downloads?: string;
  /** Where to write a video of the run. The server needs ffmpeg for it. */
  record?: string;
  /** Draw visual cursor feedback for visible local Chrome on Linux X11 with Cua Driver. */
  cursor?: boolean;
  secrets?: Secrets;
  /** Called with the address the run ended on. Anything but true keeps the run from `complete`. */
  until?: (url: string) => MaybePromise<boolean>;
  /**
   * Called with the browser event and then each step event, in the order the run produced them. Like any
   * callback here, one that throws or rejects ends the run with status `error`.
   */
  onEvent?: (event: StepEvent | BrowserEvent) => void;
  /** Called with JPEG frames of the active tab while the run goes on. Without it the page is never filmed. */
  onFrame?: (frame: Uint8Array) => void;
  /**
   * Stops the run when it aborts. `run` then rejects with `AbortError`, after the run's browser has closed.
   * A signal that has already aborted starts no run.
   */
  signal?: AbortSignal;
}

type Sparse<Fields> = { [Name in keyof Fields]?: Fields[Name] | null | undefined };

/** The fields that were given. A default must survive a per-run option that is there but undefined. */
function given<Fields>(fields: Sparse<Fields>): Sparse<Fields> {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined)) as Sparse<Fields>;
}

/** The fields that hold a value. A null is dropped here, after it has replaced the default it was given to clear. */
function held<Fields>(fields: Sparse<Fields>): Partial<Fields> {
  return Object.fromEntries(Object.entries(given(fields)).filter(([, value]) => value !== null)) as Partial<Fields>;
}

function snakeKeys<Model>(fields: CamelKeys<Model>): Model {
  return Object.fromEntries(
    Object.entries(fields).map(([name, value]) => [
      name.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`),
      value,
    ]),
  ) as Model;
}

/** The fields of a `run` request that say which browser, with a null wherever the caller gave one. */
function browserFields(options: PerRun<BrowserOptions>): Sparse<RunParams> {
  return given<RunParams>({
    local: options.local,
    chrome: options.chrome && snakeKeys<LocalChrome>(options.chrome),
    cloud_profile: options.cloudProfile,
    cdp_url: options.cdpUrl,
    cdp_port: options.cdpPort,
    attach: options.attach,
    target_match: options.targetMatch,
    foreground: options.foreground,
    proxy_country: options.proxyCountry,
    viewport: options.viewport,
    cloud_allow_resizing: options.cloudAllowResizing,
  });
}

/** The browser every run of an instance uses, as the fields of a `run` request. */
export function browserParams(options: BrowserOptions): Partial<RunParams> {
  return held<RunParams>(browserFields(options));
}

/** The `run` request for a task: the defaults from `start`, then this run's own options over them. */
export function runParams(runId: string, task: string, defaults: Partial<RunParams>, options: RunOptions): RunParams {
  return {
    ...held<RunParams>({ ...defaults, ...browserFields(options) }),
    ...held<RunParams>({
      start: options.start,
      inputs: options.inputs,
      attachments: options.attachments?.map(attachment => ({
        name: attachment.name,
        mime_type: attachment.mimeType,
        content: Buffer.from(attachment.content).toString('base64'),
      })),
      limits: options.limits && snakeKeys<Limits>(options.limits),
      authorization: options.authorization && snakeKeys<Authorization>(options.authorization),
      output_schema: options.output && wireSchema(options.output),
      downloads: options.downloads,
      record: options.record,
      cursor: options.cursor,
      secrets: options.secrets?.refs,
      // The wire carries whether a callback is held, and the callback stays here.
      frames: options.onFrame && true,
      until: options.until && true,
    }),
    run_id: runId,
    task,
  };
}
