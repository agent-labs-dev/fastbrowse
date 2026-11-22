// Loads the extension's service worker against a fake chrome API and drives its operations through the native port.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const calls = [];
const sent = [];
let onMessage;
const event = () => ({ addListener() {} });
const tabs = new Map([[7, { id: 7, url: 'https://user.test/', title: 'user tab' }]]);
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
    create: async () => {
      tabs.set(5, { id: 5, url: 'about:blank' });
      return { id: 5 };
    },
    get: async id => tabs.get(id),
    group: async () => 9,
    query: async () => [...tabs.values()],
    remove: async id => calls.push(['remove', id]),
    ungroup: async id => calls.push(['ungroup', id]),
    update: async () => ({}),
    onCreated: event(),
    onUpdated: event(),
    onRemoved: event(),
  },
  windows: { update: async () => {} },
  debugger: {
    attach: async () => {
      throw new Error('Cannot attach to this target');
    },
    detach: async ({ tabId }) => calls.push(['detach', tabId]),
    sendCommand: async () => ({}),
    onEvent: event(),
    onDetach: event(),
  },
};
const context = vm.createContext({
  chrome,
  console,
  crypto,
  navigator: { userAgent: 'Chrome/140.0.0.0' },
  fetch: async () => ({ json: async () => [] }),
});
vm.runInContext(readFileSync(process.env.BACKGROUND_JS, 'utf8'), context);

let id = 0;
async function op(op, fields = {}) {
  id += 1;
  await onMessage({ id, conn: 1, op, ...fields });
  return sent.find(message => message.id === id);
}

await op('hello', { task: 'test' });
const opened = await op('newTab', { url: 'https://example.test/' });
const adopted = await op('adopt', { tabId: 7 });
console.log(
  JSON.stringify({
    opened: opened.error,
    adopted: adopted.error,
    calls,
    listed: (await op('listTabs', { all: false })).result.tabs,
    owned: [(await op('getTab', { tabId: 5 })).error, (await op('getTab', { tabId: 7 })).error],
  }),
);
