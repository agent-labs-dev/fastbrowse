// What a caller passes to `run`, and the `run` request it becomes. Options are camelCase, and the wire keeps the
// Python models' snake_case names.

import type { Authorization, BrowserEvent, Limits, LocalChrome, RunParams, StepEvent } from './protocol.ts';

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

export interface RunOptions extends BrowserOptions {
  /** The address to start on. Without one, the first address is proposed from the task. */
  start?: string;
  inputs?: Record<string, string>;
  /** Files the run may upload. */
  attachments?: Attachment[];
  limits?: CamelKeys<Limits>;
  authorization?: CamelKeys<Authorization>;
  /** A directory that keeps the files the run downloads. */
  downloads?: string;
  /** Where to write a video of the run. The server needs ffmpeg for it. */
  record?: string;
  /** Called with the browser event and then each step event, in the order the run produced them. */
  onEvent?: (event: StepEvent | BrowserEvent) => void;
}

type Sparse<Fields> = { [Name in keyof Fields]?: Fields[Name] | undefined };

/** The fields that were given. A default must survive a per-run option that is there but undefined. */
function given<Fields>(fields: Sparse<Fields>): Partial<Fields> {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined)) as Partial<Fields>;
}

function snakeKeys<Model>(fields: CamelKeys<Model>): Model {
  return Object.fromEntries(
    Object.entries(fields).map(([name, value]) => [
      name.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`),
      value,
    ]),
  ) as Model;
}

/** The fields of a `run` request that say which browser. */
export function browserParams(options: BrowserOptions): Partial<RunParams> {
  return given<RunParams>({
    local: options.local,
    chrome: options.chrome && snakeKeys<LocalChrome>(options.chrome),
    cloud_profile: options.cloudProfile,
    cdp_url: options.cdpUrl,
    cdp_port: options.cdpPort,
    attach: options.attach,
    target_match: options.targetMatch,
    proxy_country: options.proxyCountry,
    viewport: options.viewport,
    cloud_allow_resizing: options.cloudAllowResizing,
  });
}

/** The `run` request for a task: the defaults from `start`, then this run's own options over them. */
export function runParams(runId: string, task: string, defaults: Partial<RunParams>, options: RunOptions): RunParams {
  return {
    ...defaults,
    ...browserParams(options),
    ...given<RunParams>({
      start: options.start,
      inputs: options.inputs,
      attachments: options.attachments?.map(attachment => ({
        name: attachment.name,
        mime_type: attachment.mimeType,
        content: Buffer.from(attachment.content).toString('base64'),
      })),
      limits: options.limits && snakeKeys<Limits>(options.limits),
      authorization: options.authorization && snakeKeys<Authorization>(options.authorization),
      downloads: options.downloads,
      record: options.record,
    }),
    run_id: runId,
    task,
  };
}
