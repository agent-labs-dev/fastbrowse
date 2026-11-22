// A failed adoption and a concurrent run release must both preserve the user's tab.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const calls = [];
const sent = [];
let onMessage;
let finishDetach;
let enteredDetach;
const detaching = new Promise(resolve => { enteredDetach = resolve; });
const event = () => ({ addListener() {} });
const chrome = {
  action: { setBadgeText() {}, setBadgeBackgroundColor() {} },
  alarms: { create() {}, onAlarm: event() },
  runtime: {
    connectNative: () => ({
      onMessage: { addListener: listener => (onMessage = listener) },
      onDisconnect: event(),
      postMessage: message => sent.push(message),
    }),
    getManifest: () => ({ version: '0.0.0' }),
    getURL: path => path,
    onMessage: event(),
    onStartup: event(),
    onInstalled: event(),
  },
  storage: { local: { get: async () => ({ browserId: 'b' }), set: async () => {} }, session: { get: async () => ({}), set: async () => {} } },
  tabGroups: { update: async () => {} },
  tabs: {
    get: async id => ({ id, url: 'https://user.test/', title: 'unfinished user form' }),
    group: async () => 9,
    remove: async id => calls.push(['remove', id]),
    ungroup: async id => calls.push(['ungroup', id]),
    onCreated: event(),
    onUpdated: event(),
    onRemoved: event(),
  },
  debugger: {
    attach: async () => { throw new Error('Cannot attach to this target'); },
    detach: async ({ tabId }) => {
      calls.push(['detach', tabId]);
      if (calls.length === 1) {
        enteredDetach();
        await new Promise(resolve => { finishDetach = resolve; });
      }
    },
    onEvent: event(),
    onDetach: event(),
  },
};
vm.runInContext(readFileSync(new URL('../crates/fastbrowse-browser/extension/background.js', import.meta.url), 'utf8'), vm.createContext({
  chrome, console, crypto,
  navigator: { userAgent: 'Chrome/140.0.0.0' },
  fetch: async () => ({ json: async () => [] }),
}));

await onMessage({ id: 1, conn: 1, op: 'hello', task: 'test' });
const adopting = onMessage({ id: 2, conn: 1, op: 'adopt', tabId: 7 });
await detaching;
// The native host can release the run, or the popup can stop it, while Chrome is still detaching.
await onMessage({ id: 3, conn: 1, op: 'release' });
finishDetach();
await adopting;
assert.equal(sent.find(message => message.id === 2).error, 'Cannot attach to this target');
assert.ok(calls.some(([op, id]) => op === 'ungroup' && id === 7));
assert.ok(!calls.some(([op, id]) => op === 'remove' && id === 7), `the user's tab must stay open: ${JSON.stringify(calls)}`);
