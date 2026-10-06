// The step from the models' JSON Schema to the SDK's types. `tests/test_sdk_schema.py` covers the step before it.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { render } from '../scripts/generate-types.ts';

const ping = {
  type: 'object',
  title: 'Ping',
  additionalProperties: false,
  properties: { sent_at: { type: 'number' } },
  required: ['sent_at'],
};

test('each model becomes one exported type and nothing else is exported', async () => {
  const types = await render({ $defs: { Ping: ping, Kind: { title: 'Kind', type: 'string', enum: ['a', 'b'] } } });
  assert.match(types, /export interface Ping \{\n {2}sent_at: number;\n\}/);
  assert.match(types, /export type Kind = 'a' \| 'b';/);
  assert.equal(types.match(/^export /gm)?.length, 2);
});

test('a field added to a model is in the types after they are generated again', async () => {
  const later = { ...ping, properties: { ...ping.properties, reply_to: { type: 'string' } } };
  const types = await render({ $defs: { Ping: later } });
  assert.match(types, /export interface Ping \{\n {2}sent_at: number;\n {2}reply_to\?: string;\n\}/);
});

test('a model that does not say whether it takes unknown fields gets a closed type', async () => {
  // What pydantic writes for a model without `extra="forbid"`, which is most of what the server sends.
  const { additionalProperties: _, ...unsaid } = ping;
  assert.doesNotMatch(await render({ $defs: { Ping: unsaid } }), /\[k: string\]/);
});
