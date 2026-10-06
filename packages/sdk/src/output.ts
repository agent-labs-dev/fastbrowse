// The `output` option of `run`: the JSON Schema it puts on the wire, and the typed value it makes of the data
// that comes back.

import { FastbrowseError } from './errors.ts';
import type { RunResult, Status } from './protocol.ts';

/** The extension a schema library implements to write itself as JSON Schema, by the name its specification has. */
const JSON_SCHEMA_EXTENSION = 'StandardJSONSchemaV1';

/** The draft `fastbrowse serve` reads. */
const TARGET = 'draft-2020-12';

/** One thing a schema found wrong with the data, as Standard Schema reports it. */
export interface OutputIssue {
  readonly message: string;
  readonly path?: ReadonlyArray<PropertyKey | { readonly key: PropertyKey }> | undefined;
}

type Validation<Output> =
  | { readonly value: Output; readonly issues?: undefined }
  | { readonly issues: ReadonlyArray<OutputIssue> };

/**
 * A schema that can both write itself as JSON Schema and validate: `StandardSchemaV1 & StandardJSONSchemaV1`
 * of `@standard-schema/spec`, which Zod and ArkType schemas are, and a Valibot schema is once
 * `@valibot/to-json-schema` has wrapped it.
 *
 * The two interfaces are written out here because the published declarations cannot import a package the SDK
 * does not depend on. Only the members `run` uses are named.
 */
export interface StandardOutputSchema<Output = unknown> {
  readonly '~standard': {
    readonly version: 1;
    readonly vendor: string;
    readonly types?: { readonly input: unknown; readonly output: Output } | undefined;
    readonly validate: (value: unknown) => Validation<Output> | Promise<Validation<Output>>;
    readonly jsonSchema: {
      readonly input: (options: { readonly target: typeof TARGET }) => Record<string, unknown>;
    };
  };
}

/** A JSON Schema, draft 2020-12, with an object at its root. It is sent as it is. */
export interface JsonSchema {
  readonly [keyword: string]: unknown;
  // What tells the two apart for the compiler, so a Standard Schema that lacks the extension is a type error.
  readonly '~standard'?: never;
}

export type OutputSchema = StandardOutputSchema | JsonSchema;

/** What `output` holds for a schema: its output type when the schema can validate, and otherwise `unknown`. */
export type OutputOf<Schema> = Schema extends StandardOutputSchema<infer Output> ? Output : unknown;

/**
 * The result of a run and, in `output`, its `data` as the `output` schema validated it, with the schema's
 * transforms and defaults applied. `data` stays what the server sent. With a plain JSON Schema or no schema
 * there is nothing to validate with, and `output` is `data`.
 *
 * A run that ended before it read any data has none to validate, so `output` is null there, and checking
 * `status` first leaves the schema's own type.
 */
export type TypedRunResult<Schema> = RunResult &
  (
    | { status: 'complete'; output: OutputOf<Schema> }
    | { status: Exclude<Status, 'complete'>; output: OutputOf<Schema> | null }
  );

/** The data of a run did not pass the `output` schema's own validation. */
export class OutputValidationError extends FastbrowseError {
  /** The result as the server sent it, with the data that failed in `data`. */
  readonly result: RunResult;
  readonly issues: ReadonlyArray<OutputIssue>;

  constructor(result: RunResult, issues: ReadonlyArray<OutputIssue>) {
    super(`the data of the run does not match the output schema: ${issues.map(described).join('; ')}`);
    this.result = result;
    this.issues = issues;
  }
}

function described(issue: OutputIssue): string {
  const path = (issue.path ?? []).map(segment => String(typeof segment === 'object' ? segment.key : segment));
  return path.length ? `${path.join('.')}: ${issue.message}` : issue.message;
}

function standard(schema: OutputSchema): StandardOutputSchema['~standard'] | undefined {
  return (schema as Partial<StandardOutputSchema>)['~standard'];
}

/** The JSON Schema that goes to the server as `output_schema`. */
export function wireSchema(schema: OutputSchema): Record<string, unknown> {
  const props = standard(schema);
  if (props === undefined) return schema as JsonSchema;
  // Read as a value that may be missing: the types rule that out, and a caller's cast or plain JavaScript does not.
  const convert: unknown = (props as { jsonSchema?: { input?: unknown } }).jsonSchema?.input;
  if (typeof convert !== 'function') {
    throw new FastbrowseError(
      `the output schema is a Standard Schema from "${props.vendor}" without the JSON Schema extension ` +
        `(${JSON_SCHEMA_EXTENSION}, "~standard".jsonSchema), so there is no schema to send: use a version of ` +
        'the library that has it or its JSON Schema adapter, or pass a JSON Schema object',
    );
  }
  try {
    return props.jsonSchema.input({ target: TARGET });
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    throw new FastbrowseError(`the output schema cannot be written as JSON Schema: ${reason}`, { cause: error });
  }
}

/** The result with its `output`. Rejects with `OutputValidationError` when the schema refuses the data. */
export async function withOutput<Schema extends OutputSchema>(
  result: RunResult,
  schema: Schema | undefined,
): Promise<TypedRunResult<Schema>> {
  const props = schema && standard(schema);
  const unread = result.status !== 'complete' && result.data === null;
  if (props === undefined || unread) return { ...result, output: result.data } as TypedRunResult<Schema>;
  const validation = await props.validate(result.data);
  if (validation.issues) throw new OutputValidationError(result, validation.issues);
  return { ...result, output: validation.value } as TypedRunResult<Schema>;
}
