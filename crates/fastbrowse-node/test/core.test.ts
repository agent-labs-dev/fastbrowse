// The Rust core loaded in-process as a Node addon, checked against cases from the Python tests.
// Build first: cargo build -p fastbrowse-node --release && cp target/release/libfastbrowse_node.dylib crates/fastbrowse-node/fastbrowse.node

import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';

type HostFunction<Argument> = (argument: Argument) => Promise<string>;

interface Host {
  onEvent?: HostFunction<string>;
  onFrame?: HostFunction<Buffer>;
  until?: HostFunction<string>;
  resolveSecret?: HostFunction<string>;
}

interface RunCancel {
  cancel(): void;
}

interface Core {
  originOf(url: string): string | null;
  parseOrigin(value: string): string;
  parseOrigins(values: string[]): string[];
  textFragment(url: string, text: string): string;
  OriginGrant: new (origins: string[]) => { allows(url: string): boolean };
  runTask(params: string, host?: Host, cancel?: RunCancel): Promise<string>;
  Core: new (env?: Record<string, string>) => { runTask(params: string, host?: Host, cancel?: RunCancel): Promise<string> };
  RunCancel: new () => RunCancel;
  version(): string;
  protocolVersion(): number;
}

const core = createRequire(import.meta.url)('../fastbrowse.node') as Core;

test('a grant entry is normalized to one exact origin', () => {
  assert.equal(core.parseOrigin('https://Example.COM'), 'https://example.com');
  assert.equal(core.parseOrigin('https://example.com:443/'), 'https://example.com');
  assert.equal(core.parseOrigin('http://[::1]:3000'), 'http://[::1]:3000');
  assert.equal(core.parseOrigin('https://b\u00fccher.example'), 'https://xn--bcher-kva.example');
  assert.deepEqual(core.parseOrigins(['https://A.example', 'https://a.example:443']), ['https://a.example']);
});

test('an entry that is not a bare origin is refused', () => {
  for (const entry of ['', '*', 'https://good@evil.example', 'https://example.com/app', 'https://good\\@evil.example']) {
    assert.throws(() => core.parseOrigin(entry), /allowed_origins/, entry);
  }
  assert.throws(() => core.parseOrigins([]), /empty/);
});

test('a grant allows only exact origins', () => {
  const grant = new core.OriginGrant(['https://a.example']);
  assert.equal(grant.allows('https://a.example/page?token=1#x'), true);
  // WHATWG, as Chrome: a backslash is `/`, so the `@` is in the path and the host is a.example.
  assert.equal(grant.allows('https://a.example\\@evil.example/'), true);
  assert.equal(core.originOf('https://fa\u00df.de/'), 'https://xn--fa-hia.de');
  assert.equal(grant.allows('https://a.example\t@evil.example/'), false);
  assert.equal(grant.allows('about:blank'), false);
  assert.equal(core.originOf('data:text/html,hi'), null);
});

test('text fragments encode terms and keep the anchor', () => {
  const url = 'https://example.test/a?x=1&y=2';
  assert.equal(core.textFragment(url, 'a-b & c,d'), `${url}#:~:text=a%2Db%20%26%20c%2Cd`);
  assert.equal(core.textFragment(url, 'caf\u00e9 \u6771\u4eac'), `${url}#:~:text=caf%C3%A9%20%E6%9D%B1%E4%BA%AC`);
  assert.equal(core.textFragment(url, '  a\n\tb\u00a0c  '), `${url}#:~:text=a%20b%20c`);
  assert.equal(
    core.textFragment('https://example.test/#section:~:text=old', 'new'),
    'https://example.test/#section:~:text=new',
  );
  assert.throws(() => core.textFragment('https://example.test', ' \n'), /nonempty quote/);
});

// The stand-in runs the binary's tests use, with the keys a run is refused without and a browser nothing connects to.
function scripted(run: string): InstanceType<Core['Core']> {
  const keys = { OPENROUTER_API_KEY: 'unused', AI_GATEWAY_API_KEY: 'unused', BROWSER_USE_API_KEY: 'unused' };
  return new core.Core({ ...keys, FASTBROWSE_TEST_SERVER: 'scripted_run', FASTBROWSE_TEST_RUN: run });
}

function params(fields: Record<string, unknown> = {}): string {
  return JSON.stringify({ run_id: 'r', task: 'Add the kettle', cdp_url: 'ws://127.0.0.1:9/devtools/browser/x', ...fields });
}

const ok = (value: unknown) => async () => JSON.stringify({ ok: value });

test('runTask refuses what serve refuses, with its code and message', async () => {
  const reply = JSON.parse(await core.runTask(params({ tsak: 1 })));
  assert.equal(reply.error.code, -32602);
  assert.match(reply.error.message, /^run: tsak: unknown field `tsak`/);
  assert.equal(core.protocolVersion(), 1);
});

test('a run hands each event to onEvent, in order, before it resolves', async () => {
  const events: string[] = [];
  const onEvent = async (event: string) => {
    events.push(JSON.parse(event).type);
    return JSON.stringify({ ok: null });
  };
  const reply = JSON.parse(await scripted('three_events').runTask(params(), { onEvent }));
  assert.equal(reply.result.status, 'complete');
  assert.deepEqual(events, ['browser', 'step', 'step']);
});

test('a RunCancel stops the run, which settles with the cancelled error once it has closed', async () => {
  const cancel = new core.RunCancel();
  const reply = scripted('never_ends').runTask(params(), { onEvent: async () => (cancel.cancel(), '{"ok":null}') }, cancel);
  assert.deepEqual(JSON.parse(await reply), { error: { code: -32002, message: "run 'r' was cancelled" } });
});

test('until and resolveSecret answer the run, and a failed one ends it as an error that hides what was thrown', async () => {
  const unverified = JSON.parse(await scripted('ends_on_an_order').runTask(params({ until: true }), { until: ok(false) }));
  assert.equal(unverified.result.status, 'unverified');

  const secrets = { secrets: [{ name: 'password', origins: ['https://shop.example'] }] };
  const inputs = { inputs: { name: 'password', origin: 'https://shop.example' }, ...secrets };
  const typed = JSON.parse(await scripted('types_secrets').runTask(params(inputs), { resolveSecret: ok('hunter2') }));
  assert.deepEqual(typed.result.data, ['hunter2', 'hunter2']);

  const resolveSecret = async () => JSON.stringify({ error: 'vault says hunter2 is wrong' });
  const failed = JSON.parse(await scripted('types_secrets').runTask(params(inputs), { resolveSecret }));
  assert.equal(failed.result.status, 'error');
  assert.equal(failed.result.error, "secrets/resolve: the client could not resolve the secret 'password'");
});
