export { AbortError, FastbrowseError, ProcessExitedError, RpcError } from './errors.ts';
export { type CloseOptions, Fastbrowse, type StartOptions } from './fastbrowse.ts';
// The result and the events are the wire's own shapes, so their fields keep the Python models' snake_case names.
export type {
  Artifact,
  BrowserEvent,
  BudgetStop,
  Citation,
  CostBreakdown,
  CostLine,
  ErrorCode,
  Evidence,
  RunResult,
  SecretRef,
  Status,
  StepEvent,
  StepFact,
  StepResult,
} from './protocol.ts';
export type { Attachment, BrowserOptions, RunOptions, Secrets } from './run.ts';
