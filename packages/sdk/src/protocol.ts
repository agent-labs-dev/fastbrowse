// Generated from the models in src/fastbrowse/protocol.py and src/fastbrowse/models.py.
// Do not edit: change the models, then run `npm run generate:sdk`.

export type ArtifactKind = 'download';
export type CostBasis = 'metered' | 'estimated' | 'unknown';
export type CostComponent = 'jev' | 'llm' | 'browser' | 'proxy';
export type LLMPurpose = 'plan' | 'read' | 'field_text' | 'recover' | 'verify' | 'compose' | 'shortcut';
/**
 * The model whose choice a step carries out, even when code dispatches it: the next page of a list is the
 * reader's, because the reader asked for the rest of the list.
 */
export type Decider = 'jev' | 'llm';
/**
 * Every code an error reply can carry: the JSON-RPC standard ones, then this protocol's own.
 */
export type ErrorCode = -32700 | -32600 | -32601 | -32602 | -32603 | -32001 | -32002 | -32003 | -32004;
export type RequestId = number | string;
export type FactReader = 'jev_choice' | 'llm';
export type JsonValue = unknown;
/**
 * What the server sends without being asked.
 */
export type ServerMethod = 'run/event' | 'run/frame' | 'run/until' | 'secrets/resolve';
export type Operation =
  | 'click'
  | 'hover'
  | 'drag'
  | 'fill'
  | 'select'
  | 'enter'
  | 'escape'
  | 'scroll'
  | 'scroll_up'
  | 'back'
  | 'switch_tab'
  | 'upload'
  | 'dialog'
  | 'read'
  | 'done'
  | 'escalate'
  | 'navigate';
export type StepOutcome = 'executed' | 'covered' | 'stale' | 'failed';
export type Status =
  | 'complete'
  | 'unverified'
  | 'needs_confirmation'
  | 'needs_login'
  | 'blocked'
  | 'needs_input'
  | 'stuck'
  | 'budget_exceeded'
  | 'observation_limit'
  | 'unavailable'
  | 'error';
export type Tripwire = 'no_progress' | 'action_repetition' | 'plan_stagnation';
export interface Artifact {
  kind: ArtifactKind;
  name: string;
  mime_type: string;
  size_bytes: number;
  sha256: string;
  /**
   * Reference returned by the caller's `ArtifactSink`; fastbrowse never keeps bytes past the run.
   */
  uri: string;
}
export interface Authorization {
  /**
   * Allow submit/pay/delete/send style actions without pausing for confirmation.
   */
  irreversible_actions?: boolean;
}
/**
 * Sent once, when the browser is open and before the first step.
 */
export interface BrowserEvent {
  type: 'browser';
  /**
   * Where a cloud browser can be watched live; None for local Chrome.
   */
  live_url: string | null;
  /**
   * The cloud browser's id, for a caller that may have to stop it out of band. None when the run did not
   * start a browser of its own (local Chrome, or one handed over through `cdp_url`).
   */
  browser_id: string | null;
}
export interface BudgetStop {
  resource: 'steps' | 'dollars' | 'seconds' | 'jev_calls' | 'llm_calls';
  limit: number;
}
export interface Citation {
  /**
   * The number used by this fact's links in the answer, stable within a run.
   */
  id: number;
  text: string;
  requirement_id: string | null;
  url: string;
  quote: string;
  deep_link: string;
}
export interface CostBreakdown {
  lines: CostLine[];
}
export interface CostLine {
  component: CostComponent;
  basis: CostBasis;
  dollars: number | null;
  purpose: LLMPurpose | null;
  input_tokens: number;
  output_tokens: number;
  /**
   * Wall time of the call, retries included. None for a line that is not one call, such as browser time.
   */
  seconds: number | null;
}
export interface Error {
  code: ErrorCode;
  message: string;
}
export interface ErrorResponse {
  jsonrpc: '2.0';
  id: RequestId | null;
  error: Error;
}
export interface Evidence {
  source_id: string;
  url: string;
  frame_id: string | null;
  captured_at: string;
  capture_sha256: string;
  start: number;
  end: number;
  quote: string;
  /**
   * False for typed DOM observations whose quote is not literal rendered page text.
   */
  rendered_text: boolean;
  /**
   * The headings the quote sits under on the page: which record a bare "£10.69" is the price of.
   */
  heading_path: string[];
}
/**
 * The client's side of the handshake.
 *
 * Unknown fields are allowed here and nowhere else: a client on a later protocol may send some, and it has to
 * get the server's version back to report the mismatch, which a refusal of its params would hide.
 */
export interface InitializeParams {
  protocol_version: number;
}
export interface InitializeResult {
  protocol_version: number;
  fastbrowse_version: string;
}
export interface Limits {
  /**
   * Steps that reached the page. A click whose element was redrawn before it landed dispatched nothing and is
   * not counted; the stall budget ends a page that keeps redrawing.
   */
  max_steps?: number | null;
  /**
   * Logical Jev evaluations. A hedged or retried request adds cost but not a call.
   */
  max_jev_calls?: number | null;
  max_llm_calls?: number | null;
  /**
   * Bounds Jev and LLM spend as it happens. A cloud browser bills when it stops, after the run, so its
   * cost is reported in the result but cannot stop the run that incurred it.
   */
  max_dollars?: number | null;
  /**
   * Stops active work at the time budget. Returning also waits for owned request and browser cleanup.
   */
  max_seconds?: number | null;
}
export interface LocalChrome {
  /**
   * A name or path that replaces discovery of the Chrome binary.
   */
  binary?: string | null;
  /**
   * Show the window, to watch a run.
   */
  headed?: boolean;
  /**
   * A profile directory kept between runs, so a site signed into there stays signed in. Without one,
   * every run starts from a fresh profile that is deleted afterwards.
   */
  profile?: string | null;
}
export interface NoParams {}
/**
 * A message from the server that takes no reply. It has no `id` member at all, which is what marks it.
 */
export interface Notification {
  jsonrpc: '2.0';
  method: ServerMethod;
  params: unknown;
}
/**
 * A method's params. An unknown field is refused, so a misspelled option is never dropped in silence.
 */
export interface Params {}
/**
 * A call in either direction. One without an id is a notification and gets no reply.
 */
export interface Request {
  jsonrpc: '2.0';
  id: RequestId | null;
  method: string;
  params:
    | {
        [k: string]: unknown;
      }
    | unknown[]
    | null;
}
export interface Response {
  jsonrpc: '2.0';
  id: RequestId;
  result: unknown;
}
/**
 * An `Attachment` as it travels: JSON has no bytes, so the content is base64.
 */
export interface RunAttachment {
  name: string;
  mime_type: string;
  content: string;
}
/**
 * Which run to stop. One that is not active, because it finished or never was, is left as it is.
 *
 * The reply says only that the request was read. The run's own reply is what says it was cancelled: the
 * `cancelled` error, written after its browser has closed.
 */
export interface RunCancelParams {
  run_id: string;
}
/**
 * The params of `run/event`: one thing a run did, written before that run's reply.
 */
export interface RunEvent {
  run_id: string;
  event: StepEvent | BrowserEvent;
}
export interface StepEvent {
  type: 'step';
  step: StepResult;
  /**
   * A PNG of the page this step acted on, when `Config.step_frames` asked for one. None when it did not,
   * and also when a resolved secret was showing as page text: pixels cannot be masked the way text is.
   */
  frame: string | null;
}
export interface StepResult {
  index: number;
  operation: Operation;
  decided_by: Decider;
  outcome: StepOutcome;
  url: string;
  /**
   * Human-readable label of the chosen element; never contains a secret value.
   */
  target: string | null;
  confidence: number | null;
  note: string | null;
  /**
   * Facts added to the notes by this step, with resolved secrets redacted.
   */
  facts: StepFact[];
  /**
   * Whether the page's fingerprint changed; None for steps that do not act (read, escalate).
   */
  page_changed: boolean | null;
  duration_ms: number;
}
export interface StepFact {
  text: string;
  requirement_id: string | null;
  /**
   * None for a count, total or winner derived from other facts rather than read from the page.
   */
  quote: string | null;
  url: string | null;
  reader: FactReader;
  deep_link: string | null;
}
/**
 * The params of `run/frame`: the active tab as it looked a moment ago. Sent only to a run that set `frames`.
 */
export interface RunFrame {
  run_id: string;
  frame: string;
}
/**
 * One run. Apart from `run_id`, each field is the `run_task` argument or the CLI flag of the same name.
 *
 * The reply is the `RunResult`. No API key is among the fields: model and browser keys come from the
 * environment the server inherited.
 */
export interface RunParams {
  run_id: string;
  task: string;
  start?: string | null;
  inputs?: {
    [k: string]: string;
  } | null;
  attachments?: RunAttachment[];
  limits?: Limits | null;
  authorization?: Authorization | null;
  output_schema?: {
    [k: string]: unknown;
  } | null;
  secrets?: SecretRef[];
  frames?: boolean;
  until?: boolean;
  downloads?: string | null;
  record?: string | null;
  local?: boolean;
  chrome?: LocalChrome | null;
  cloud_profile?: string | null;
  cdp_url?: string | null;
  cdp_port?: number | null;
  attach?: boolean;
  target_match?: string | null;
  proxy_country?: string | null;
  viewport?: [number, number] | null;
  cloud_allow_resizing?: boolean;
}
export interface SecretRef {
  name: string;
  /**
   * Origins (scheme://host[:port]) the value may be typed into; models only ever see `name`.
   */
  origins: string[];
}
export interface RunResult {
  status: Status;
  budget: BudgetStop | null;
  answer: string | null;
  data: unknown;
  evidence: Evidence[];
  steps: StepResult[];
  cost: CostBreakdown;
  artifacts: Artifact[];
  citations: Citation[];
  error: string | null;
  /**
   * Where the browser was last observed; what a caller checks when the task was to arrive somewhere.
   */
  final_url: string | null;
  /**
   * Shadow tripwires retain each occurrence so eval counts do not depend on logging configuration.
   */
  would_fire: Tripwire[];
  /**
   * The videos this run finished writing, captioned then plain; empty when it recorded nothing or encoding failed.
   */
  recordings: string[];
}
/**
 * The params of `run/until`: the address a run ended on. The reply is a boolean, and false keeps the run
 * from `complete`.
 */
export interface RunUntilParams {
  run_id: string;
  url: string;
}
/**
 * The params of `secrets/resolve`: one value the run is about to type. The reply is the value, or null.
 */
export interface SecretsResolveParams {
  run_id: string;
  name: string;
  origin: string;
}
/**
 * A call from the server that the client answers, with a `Response` or an `ErrorResponse` carrying its id.
 *
 * The ids are the server's own and count up from one. A client's ids may be the same numbers: a reply is
 * told from a request by having no `method`, so the two never meet.
 */
export interface ServerRequest {
  jsonrpc: '2.0';
  id: number;
  method: ServerMethod;
  params: unknown;
}
export const ErrorCode = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  BUSY: -32001,
  CANCELLED: -32002,
  CONFIGURATION: -32003,
  UNSUPPORTED_SCHEMA: -32004,
} as const;
export const Method = {
  INITIALIZE: 'initialize',
  RUN: 'run',
  RUN_CANCEL: 'run/cancel',
  SHUTDOWN: 'shutdown',
} as const;
export const PROTOCOL_VERSION = 1;
export const ServerMethod = {
  RUN_EVENT: 'run/event',
  RUN_FRAME: 'run/frame',
  RUN_UNTIL: 'run/until',
  SECRETS_RESOLVE: 'secrets/resolve',
} as const;
