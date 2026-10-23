// The Rust core loaded in-process as a Node addon, checked against cases from the Python tests.
// Build first: cargo build -p fastbrowse-node --release && cp target/release/libfastbrowse_node.dylib crates/fastbrowse-node/fastbrowse.node

import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';

interface Core {
  originOf(url: string): string | null;
  parseOrigin(value: string): string;
  parseOrigins(values: string[]): string[];
  textFragment(url: string, text: string): string;
  OriginGrant: new (origins: string[]) => { allows(url: string): boolean };
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
