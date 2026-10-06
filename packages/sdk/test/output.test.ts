// The `output` option of `run`: the schema that goes to the server, and the typed value that comes back.

import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { test } from 'node:test';

import type { StandardJSONSchemaV1, StandardSchemaV1 } from '@standard-schema/spec';
import { type } from 'arktype';
import * as v from 'valibot';
import { z } from 'zod';

import {
  Fastbrowse,
  FastbrowseError,
  type JsonSchema,
  type OutputOf,
  type OutputSchema,
  OutputValidationError,
  RpcError,
  type RunOptions,
  type RunResult,
  type StandardOutputSchema,
  type StartOptions,
} from '../src/index.ts';
import { scriptedRun } from './server.ts';

// The code `ErrorCode` gives it in `src/fastbrowse/protocol.py`.
const UNSUPPORTED_SCHEMA = -32004;

// A browser the caller already runs, so no run here is refused for a missing Chrome. Nothing connects to it.
const ownBrowser = { cdpUrl: 'ws://127.0.0.1:9222/devtools/browser/none' };

type Equal<Left, Right> = (<T>() => T extends Left ? 1 : 2) extends <T>() => T extends Right ? 1 : 2 ? true : false;

async function withServer<T>(options: StartOptions, use: (fb: Fastbrowse) => Promise<T>): Promise<T> {
  const fb = await Fastbrowse.start(options);
  try {
    return await use(fb);
  } finally {
    await fb.close();
  }
}

/** The options of a run on the `found` server that finds this data on the page. */
function finding(found: unknown) {
  return { ...ownBrowser, inputs: { found: JSON.stringify(found) } };
}

const order = {
  id: 'A-17',
  placed: '2026-03-04T10:00:00.000Z',
  customer: { name: 'Ada', vip: true },
  lines: [
    { sku: 'kettle', quantity: 2, tags: ['kitchen', 'steel'] },
    { sku: 'mug', quantity: 6, tags: [] },
  ],
};

const Order = z.object({
  id: z.string().describe('The order number on the confirmation page'),
  placed: z.string().transform(written => new Date(written)),
  customer: z.object({ name: z.string(), vip: z.boolean() }),
  lines: z.array(z.object({ sku: z.string(), quantity: z.int(), tags: z.array(z.string()) })),
  currency: z.string().default('EUR'),
});

test('a Zod schema types the output, and the data comes back through its validation and transforms', async () => {
  await withServer(scriptedRun('found'), async fb => {
    const result = await fb.run('Read the order', { ...finding(order), output: Order });

    assert.equal(result.status, 'complete');
    if (result.status !== 'complete') return;
    const typed: Equal<
      typeof result.output,
      {
        id: string;
        placed: Date;
        customer: { name: string; vip: boolean };
        lines: { sku: string; quantity: number; tags: string[] }[];
        currency: string;
      }
    > = true;
    assert.ok(typed);
    assert.deepEqual(result.output, { ...order, placed: new Date('2026-03-04T10:00:00.000Z'), currency: 'EUR' });
    // The wire value stays as the server sent it.
    assert.deepEqual(result.data, order);
  });
});

test('the JSON Schema a Zod schema writes for its input is what the server is sent', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    // The echo is not an order, so it comes back through a schema that writes what `Order` does and takes anything.
    const writesAsOrder = {
      '~standard': {
        version: 1 as const,
        vendor: 'test',
        validate: (value: unknown) => ({ value: value as { params: Record<string, unknown> } }),
        jsonSchema: Order['~standard'].jsonSchema,
      },
    };
    const result = await fb.run('Read the order', { ...ownBrowser, output: writesAsOrder });

    assert.deepEqual(result.output?.params.output_schema, {
      $schema: 'https://json-schema.org/draft/2020-12/schema',
      type: 'object',
      properties: {
        id: { type: 'string', description: 'The order number on the confirmation page' },
        // What the transform takes, and not the date it returns.
        placed: { type: 'string' },
        customer: {
          type: 'object',
          properties: { name: { type: 'string' }, vip: { type: 'boolean' } },
          required: ['name', 'vip'],
        },
        lines: {
          type: 'array',
          items: {
            type: 'object',
            properties: {
              sku: { type: 'string' },
              quantity: { type: 'integer', minimum: -9007199254740991, maximum: 9007199254740991 },
              tags: { type: 'array', items: { type: 'string' } },
            },
            required: ['sku', 'quantity', 'tags'],
          },
        },
        currency: { default: 'EUR', type: 'string' },
      },
      required: ['id', 'placed', 'customer', 'lines'],
    });
  });
});

test('an ArkType schema types the output and validates the data', async () => {
  const Line = type({ sku: 'string', quantity: 'number.integer', 'note?': 'string | null', size: "'small' | 'large'" });
  const Basket = type({ lines: Line.array(), total: 'number' });
  const basket = { lines: [{ sku: 'kettle', quantity: 2, note: null, size: 'large' }], total: 59.9 };

  await withServer(scriptedRun('found'), async fb => {
    const result = await fb.run('Read the basket', { ...finding(basket), output: Basket });

    assert.equal(result.status, 'complete');
    if (result.status !== 'complete') return;
    const typed: Equal<
      typeof result.output,
      { lines: { sku: string; quantity: number; size: 'small' | 'large'; note?: string | null }[]; total: number }
    > = true;
    assert.ok(typed);
    assert.deepEqual(result.output, basket);
  });
});

const orderSchema = {
  $schema: 'https://json-schema.org/draft/2020-12/schema',
  type: 'object',
  properties: {
    id: { type: 'string' },
    customer: { $ref: '#/$defs/customer' },
    lines: {
      type: 'array',
      items: {
        type: 'object',
        properties: { sku: { type: 'string' }, quantity: { type: 'integer' }, tags: { type: 'array' } },
        required: ['sku', 'quantity'],
        additionalProperties: false,
      },
    },
  },
  required: ['id', 'customer', 'lines'],
  $defs: { customer: { type: 'object', properties: { name: { type: 'string' }, vip: { type: 'boolean' } } } },
};

test('a plain JSON Schema is sent as it is', async () => {
  await withServer(scriptedRun('echo'), async fb => {
    const result = await fb.run('Read the order', { ...ownBrowser, output: orderSchema });

    const { params } = result.data as { params: Record<string, unknown> };
    assert.deepEqual(params.output_schema, orderSchema);
  });
});

test('with a plain JSON Schema, nested data round-trips and output is the data, typed unknown', async () => {
  const { placed: _, ...found } = order;
  await withServer(scriptedRun('found'), async fb => {
    const result = await fb.run('Read the order', { ...finding(found), output: orderSchema });

    const untyped: Equal<typeof result.output, unknown> = true;
    assert.ok(untyped);
    assert.deepEqual(result.output, found);
    assert.deepEqual(result.data, found);
  });
});

test('without an output schema, output is the data, typed unknown', async () => {
  await withServer(scriptedRun('request_count'), async fb => {
    const result = await fb.run('Read the order', ownBrowser);

    const untyped: Equal<typeof result.output, unknown> = true;
    assert.ok(untyped);
    assert.equal(result.output, 1);

    // Options that were built somewhere else say only that there may be a schema of either kind.
    const options: RunOptions = { ...ownBrowser, output: orderSchema };
    const later = await fb.run('Read the order', options);
    const alsoUntyped: Equal<typeof later.output, unknown> = true;
    assert.ok(alsoUntyped);
  });
});

test('a Standard Schema without the JSON Schema extension rejects before anything is sent', async () => {
  const plainValibot = v.object({ id: v.string() });
  await withServer(scriptedRun('request_count'), async fb => {
    // @ts-expect-error The types refuse it as well, for a caller who has them.
    const refused = fb.run('Read the order', { ...ownBrowser, output: plainValibot });

    await assert.rejects(refused, (error: unknown) => {
      assert.ok(error instanceof FastbrowseError);
      assert.match(error.message, /StandardJSONSchemaV1/);
      assert.match(error.message, /valibot/);
      return true;
    });
    // The next run is the first the server has heard of.
    assert.equal((await fb.run('Read the order', ownBrowser)).data, 1);
  });
});

test('a schema its library cannot write as JSON Schema rejects before anything is sent', async () => {
  await withServer(scriptedRun('request_count'), async fb => {
    const refused = fb.run('Read the order', { ...ownBrowser, output: z.object({ placed: z.date() }) });

    await assert.rejects(refused, (error: unknown) => {
      assert.ok(error instanceof FastbrowseError);
      assert.match(error.message, /cannot be written as JSON Schema: Date cannot be represented/);
      return true;
    });
    assert.equal((await fb.run('Read the order', ownBrowser)).data, 1);
  });
});

test('data the schema refuses rejects with a validation error that carries the result', async () => {
  // A rule JSON Schema has no word for, so the server cannot have applied it and only the schema can.
  const Numbered = z.object({ id: z.string().refine(id => id.startsWith('A-'), 'an order number starts with A-') });

  await withServer(scriptedRun('found'), async fb => {
    const refused = fb.run('Read the order', { ...finding({ id: 'B-17' }), output: Numbered });

    await assert.rejects(refused, (error: unknown) => {
      assert.ok(error instanceof OutputValidationError);
      assert.ok(error instanceof FastbrowseError);
      const result: RunResult = error.result;
      assert.equal(result.status, 'complete');
      assert.deepEqual(result.data, { id: 'B-17' });
      assert.deepEqual(
        error.issues.map(issue => [issue.path, issue.message]),
        [[['id'], 'an order number starts with A-']],
      );
      assert.match(error.message, /id: an order number starts with A-/);
      return true;
    });
  });
});

test('a keyword the server cannot enforce rejects with its keyword and path', async () => {
  const Coded = z.object({ lines: z.array(z.object({ sku: z.string().regex(/^[a-z]+$/) })) });

  await withServer(scriptedRun('found'), async fb => {
    const refused = fb.run('Read the order', { ...finding({ lines: [] }), output: Coded });

    await assert.rejects(refused, (error: unknown) => {
      assert.ok(error instanceof RpcError);
      assert.equal(error.code, UNSUPPORTED_SCHEMA);
      assert.match(error.message, /"pattern" at #\/properties\/lines\/items\/properties\/sku\/pattern/);
      return true;
    });
  });
});

test('a run that ended before it read any data resolves with a null output', async () => {
  await withServer(scriptedRun('needs_login'), async fb => {
    const result = await fb.run('Read my orders', { ...ownBrowser, output: Order });

    assert.equal(result.status, 'needs_login');
    const nullable: Equal<typeof result.output, z.output<typeof Order> | null> = true;
    assert.ok(nullable);
    assert.equal(result.output, null);
  });
});

test('the schema types are the ones of @standard-schema/spec, which the built package does not import', () => {
  // The specification's own two interfaces fit the option together, and a validator alone does not.
  type Both<Output> = StandardSchemaV1<unknown, Output> & StandardJSONSchemaV1<unknown, Output>;
  const fits: [
    Both<{ id: string }> extends StandardOutputSchema<{ id: string }> ? true : false,
    Equal<OutputOf<Both<{ id: string }>>, { id: string }>,
    StandardSchemaV1<unknown, string> extends OutputSchema ? true : false,
    Equal<OutputOf<JsonSchema>, unknown>,
  ] = [true, true, false, true];
  assert.ok(fits);

  const built = new URL('../dist/', import.meta.url);
  const imported = readdirSync(built).flatMap(name =>
    Array.from(readFileSync(new URL(name, built), 'utf8').matchAll(/from '([^']+)'/g), found => found[1] ?? ''),
  );
  assert.ok(imported.length > 0);
  assert.deepEqual(
    imported.filter(specifier => !specifier.startsWith('./') && !specifier.startsWith('node:')),
    [],
  );
});
