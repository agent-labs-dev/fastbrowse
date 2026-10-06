// Writes `src/protocol.ts`, the SDK's types for everything on the wire, from the Python models.
//
//     npm run generate:sdk
//
// `scripts/sdk_schema.py` prints the models as JSON Schema, and json-schema-to-typescript turns that into types.
// The protocol version, the error codes and the method names come with it and are written as values, so the
// SDK holds no copy of one that could fall behind the server's.

import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { compile, type JSONSchema } from 'json-schema-to-typescript';

const repository = fileURLToPath(new URL('../../../', import.meta.url));
const output = fileURLToPath(new URL('../src/protocol.ts', import.meta.url));

const BANNER = `// Generated from the models in src/fastbrowse/protocol.py and src/fastbrowse/models.py.
// Do not edit: change the models, then run \`npm run generate:sdk\`.`;

const ROOT = 'GeneratedRoot';

type Constant = number | string;

/** What `scripts/sdk_schema.py` prints: the models as JSON Schema, and the values both sides have to agree on. */
type Document = JSONSchema & { constants?: Record<string, Constant | Record<string, Constant>> };

function literal(value: Constant): string {
  return typeof value === 'string' ? `'${value}'` : String(value);
}

/** One exported value per constant. An enum is an object of its members, which may share its type's name. */
function renderConstants(constants: NonNullable<Document['constants']>): string {
  return Object.entries(constants)
    .map(([name, value]) => {
      if (typeof value !== 'object') return `export const ${name} = ${literal(value)};\n`;
      const members = Object.entries(value).map(([member, held]) => `  ${member}: ${literal(held)},\n`);
      return `export const ${name} = {\n${members.join('')}} as const;\n`;
    })
    .join('');
}

/**
 * The types for a schema whose `$defs` hold the models, and the values of its `constants`. Each definition
 * becomes one exported type.
 */
export async function render({ constants = {}, ...schema }: Document): Promise<string> {
  const names = Object.keys(schema.$defs ?? {});
  // The compiler writes a type for the root and for what the root refers to. The root here refers to every
  // definition so that each is written, and its own type, which describes no message, is taken out again.
  const root = {
    ...schema,
    title: ROOT,
    type: 'object',
    properties: Object.fromEntries(names.map(name => [name, { $ref: `#/$defs/${name}` }])),
  } satisfies JSONSchema;
  const types = await compile(root, ROOT, {
    // A model that does not forbid extra fields would otherwise get an index signature, and a misspelled
    // `event.tpye` would then compile.
    additionalProperties: false,
    bannerComment: BANNER,
    style: { singleQuote: true, printWidth: 120 },
  });
  return types.replace(new RegExp(`\nexport interface ${ROOT} \\{[^}]*\\}\n`), '') + renderConstants(constants);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const schema = execFileSync('uv', ['run', 'python', 'scripts/sdk_schema.py'], { cwd: repository, encoding: 'utf8' });
  writeFileSync(output, await render(JSON.parse(schema)));
}
