// The fastbrowse extension's service worker: relays CDP between fastbrowse runs and this browser's tabs.
//
// Runs reach it through `fastbrowse native-host`, which Chrome starts when this worker calls connectNative. Each run
// is a connection id (`conn`) the host assigns. A run works only in tabs it opens, inside a tab group of its own, and
// only with the CDP methods in allowed-methods.json: nothing here reads cookies, storage or saved passwords.

const HOST = 'dev.fastbrowse.host';
const PROTOCOL = 1;
const COLORS = ['blue', 'cyan', 'green', 'purple', 'pink', 'orange', 'yellow', 'red'];

let port = null;
let allowed = null;
/** conn -> { conn, task, started, groupId, tabs: Set<tabId>, adopted: Set<tabId>, color, released, ending } */
const runs = new Map();
/** tabId -> conn */
const owners = new Map();
/** tabId -> the last Fetch.enable params, copied to a popup the tab opens so its first request is caught early. */
const fetchParams = new Map();
/** Tabs a run asked to close, so their removal is not read as the user stopping the run. */
const closing = new Set();
/** Chunked messages from the host being joined: id -> parts. */
const chunks = new Map();
/** How many finished runs the popup lists. */
const RECENT = 5;

async function identity() {
  const stored = await chrome.storage.local.get(['browserId', 'label']);
  let browserId = stored.browserId;
  if (!browserId) {
    browserId = crypto.randomUUID().replaceAll('-', '').slice(0, 16);
    await chrome.storage.local.set({ browserId });
  }
  const brands = navigator.userAgentData?.brands?.map(b => b.brand) ?? [];
  let browser = 'Chrome';
  if (brands.includes('Microsoft Edge')) browser = 'Edge';
  else if (navigator.brave) browser = 'Brave';
  else if (!brands.includes('Google Chrome')) browser = 'Chromium';
  const version = (navigator.userAgent.match(/Chrome\/([\d.]+)/) || [])[1] || '';
  return { browserId, label: stored.label || browser, browser, version, userAgent: navigator.userAgent };
}

async function allowedMethods() {
  if (!allowed) {
    const response = await fetch(chrome.runtime.getURL('allowed-methods.json'));
    allowed = new Set(await response.json());
  }
  return allowed;
}

function send(message) {
  port?.postMessage(message);
}

function updateBadge() {
  const count = runs.size;
  chrome.action.setBadgeText({ text: count ? String(count) : '' });
  chrome.action.setBadgeBackgroundColor({ color: '#c4451b' });
}

function runOf(conn) {
  const run = runs.get(conn);
  if (!run) throw new Error('this run has not said hello');
  return run;
}

function ownedTab(conn, tabId) {
  if (owners.get(tabId) !== conn) throw new Error(`tab ${tabId} does not belong to this run`);
  return tabId;
}

async function addToGroup(run, tabId) {
  if (run.groupId === undefined) {
    run.groupId = await chrome.tabs.group({ tabIds: [tabId] });
    const title = `fastbrowse: ${run.task}`.slice(0, 60);
    await chrome.tabGroups.update(run.groupId, { title, color: run.color });
  } else {
    await chrome.tabs.group({ groupId: run.groupId, tabIds: [tabId] });
  }
}

async function adoptTab(conn, run, tabId) {
  run.tabs.add(tabId);
  owners.set(tabId, conn);
  try {
    await addToGroup(run, tabId);
    await chrome.debugger.attach({ tabId }, '1.3');
  } finally {
    // Stopped meanwhile: the release may have passed this tab before the attach landed, so undo it here.
    if (run.released) await releaseTab(run, tabId);
  }
  if (run.released) throw new Error('the run was stopped');
}

function tabSummary(tab) {
  return { tabId: tab.id, url: tab.url || tab.pendingUrl || '', title: tab.title || '', openerTabId: tab.openerTabId };
}

const ops = {
  async hello(conn, message) {
    // A run on another protocol is answered anyway: it compares versions and names which side to update.
    const run = runs.get(conn) || {
      task: String(message.task || 'run').trim() || 'run',
      conn,
      started: Date.now(),
      tabs: new Set(),
      adopted: new Set(),
      color: COLORS[runs.size % COLORS.length],
    };
    runs.set(conn, run);
    updateBadge();
    return { protocol: PROTOCOL, ...(await identity()) };
  },
  async ping() {
    return {};
  },
  async newTab(conn, { url, background }) {
    const run = runOf(conn);
    const tab = await chrome.tabs.create({ url: 'about:blank', active: !background });
    await adoptTab(conn, run, tab.id);
    if (url && url !== 'about:blank') await chrome.tabs.update(tab.id, { url });
    return { tabId: tab.id };
  },
  async adopt(conn, { tabId }) {
    const run = runOf(conn);
    if (owners.has(tabId)) throw new Error(`tab ${tabId} belongs to another run`);
    const tab = await chrome.tabs.get(tabId);
    run.adopted.add(tabId);
    await adoptTab(conn, run, tabId);
    return tabSummary(tab);
  },
  async listTabs(conn, { all }) {
    const run = runOf(conn);
    const tabs = await chrome.tabs.query({});
    return { tabs: tabs.filter(t => all || run.tabs.has(t.id)).map(tabSummary) };
  },
  async getTab(conn, { tabId }) {
    return tabSummary(await chrome.tabs.get(ownedTab(conn, tabId)));
  },
  async closeTab(conn, { tabId }) {
    closing.add(ownedTab(conn, tabId));
    await chrome.tabs.remove(tabId);
    return {};
  },
  async activate(conn, { tabId }) {
    const tab = await chrome.tabs.update(ownedTab(conn, tabId), { active: true });
    await chrome.windows.update(tab.windowId, { focused: true });
    return {};
  },
  async cdp(conn, { tabId, sessionId, method, params }) {
    ownedTab(conn, tabId);
    if (!(await allowedMethods()).has(method)) throw new Error(`${method} is not relayed by the fastbrowse extension`);
    if (method === 'Fetch.enable' && !sessionId) fetchParams.set(tabId, params);
    const target = sessionId ? { tabId, sessionId } : { tabId };
    return (await chrome.debugger.sendCommand(target, method, params || {})) ?? {};
  },
  async release(conn) {
    await releaseRun(conn, 'ended');
    return null;
  },
  // The run lost a tab to another extension's frame (#822), which fastbrowse saw load just before Chrome detached it.
  async taken(conn, { origin }) {
    const run = runs.get(conn);
    if (run) {
      run.ending ??= 'other_extension';
      run.taker ??= origin;
    }
    return null;
  },
};

/** Stop relaying for a run and close its tabs. `ending` is how it ended, for the popup's recent runs. */
async function releaseRun(conn, ending) {
  const run = runs.get(conn);
  if (!run) return;
  runs.delete(conn);
  run.released = true;
  updateBadge();
  await remember(run, run.ending || ending);
  for (const tabId of [...run.tabs]) await releaseTab(run, tabId);
}

// Runs that end together (Stop all, the host going away) each read and write the list, so the writes queue.
let remembering = Promise.resolve();
function remember(run, ending) {
  remembering = remembering
    .then(async () => {
      const { recent = [] } = await chrome.storage.session.get('recent');
      recent.unshift({ task: run.task, ending, taker: run.taker, started: run.started, ended: Date.now() });
      await chrome.storage.session.set({ recent: recent.slice(0, RECENT) });
    })
    .catch(() => {});
  return remembering;
}

/** A stop the user asked for in the browser: tell the run why, and note how it ended. */
function stopRun(conn, by) {
  const run = runs.get(conn);
  if (run) run.ending ??= by;
  send({ conn, event: 'stop', by });
}

// The tab stays owned until its teardown ends, so no other run can adopt it meanwhile.
async function releaseTab(run, tabId) {
  fetchParams.delete(tabId);
  await chrome.debugger.detach({ tabId }).catch(() => {});
  if (run.adopted.has(tabId)) {
    await chrome.tabs.ungroup(tabId).catch(() => {});
  } else {
    closing.add(tabId);
    await chrome.tabs.remove(tabId).catch(() => {});
    closing.delete(tabId);
  }
  if (owners.get(tabId) === run.conn) owners.delete(tabId);
}

async function handle(raw) {
  let message = raw;
  if (raw.chunk) {
    const { id, index, count, data } = raw.chunk;
    const parts = chunks.get(id) || [];
    parts[index] = data;
    chunks.set(id, parts);
    if (parts.filter(p => p !== undefined).length < count) return;
    chunks.delete(id);
    message = JSON.parse(parts.join(''));
  }
  const { id, conn, op } = message;
  const handler = ops[op];
  try {
    if (!handler) throw new Error(`unknown operation ${op}`);
    const result = await handler(conn, message);
    if (result !== null) send({ id, conn, result });
  } catch (error) {
    send({ id, conn, error: String(error?.message || error) });
  }
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  const conn = owners.get(source.tabId);
  if (conn === undefined) return;
  send({ conn, event: 'cdp', tabId: source.tabId, sessionId: source.sessionId, method, params });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  const conn = owners.get(source.tabId);
  if (conn === undefined) return;
  if (reason === 'canceled_by_user') {
    // Cancel on Chrome's "started debugging this browser" bar detaches every session: every run stops.
    for (const running of [...runs.keys()]) stopRun(running, 'debugging_bar');
    return;
  }
  send({ conn, event: 'detached', tabId: source.tabId, reason });
});

chrome.tabs.onCreated.addListener(async tab => {
  const conn = tab.openerTabId === undefined ? undefined : owners.get(tab.openerTabId);
  if (conn === undefined) return;
  const run = runs.get(conn);
  if (!run) return;
  // A popup of the run's tab. Attach and copy the opener's request interception at once, before the run hears of it,
  // so its first request is most likely caught.
  try {
    run.tabs.add(tab.id);
    owners.set(tab.id, conn);
    await chrome.debugger.attach({ tabId: tab.id }, '1.3');
    const fetch = fetchParams.get(tab.openerTabId);
    if (fetch) {
      await chrome.debugger.sendCommand({ tabId: tab.id }, 'Fetch.enable', fetch);
      fetchParams.set(tab.id, fetch);
    }
    await addToGroup(run, tab.id);
  } catch {
    // A tab that cannot be debugged (a chrome:// page) is still reported; commands to it fail with the reason.
  }
  send({ conn, event: 'popup', tabId: tab.id, openerTabId: tab.openerTabId, url: tab.pendingUrl || tab.url || '' });
});

chrome.tabs.onUpdated.addListener((tabId, change) => {
  const conn = owners.get(tabId);
  if (conn === undefined || (change.url === undefined && change.title === undefined)) return;
  send({ conn, event: 'tabUpdated', tabId, url: change.url, title: change.title });
});

chrome.tabs.onRemoved.addListener(tabId => {
  const conn = owners.get(tabId);
  if (conn === undefined) return;
  owners.delete(tabId);
  fetchParams.delete(tabId);
  const run = runs.get(conn);
  run?.tabs.delete(tabId);
  send({ conn, event: 'tabRemoved', tabId });
  const byRun = closing.delete(tabId);
  // The user closed the last of a run's tabs: that stops the run.
  if (!byRun && run && run.tabs.size === 0) stopRun(conn, 'last_tab_closed');
});

/** What the popup shows: whether the host is connected, each run with the page it is on, and recent runs. */
async function overview() {
  const listed = [];
  for (const [conn, run] of runs) {
    const tabs = await Promise.all([...run.tabs].map(tabId => chrome.tabs.get(tabId).catch(() => null)));
    const tab = tabs.find(t => t?.active) || tabs.find(Boolean);
    let host = '';
    try {
      host = new URL(tab?.url || '').host;
    } catch {}
    listed.push({ conn, task: run.task, tabs: run.tabs.size, started: run.started, title: tab?.title || '', host });
  }
  const { recent = [] } = await chrome.storage.session.get('recent');
  const { browserId } = await identity();
  return { connected: port !== null, browserId, runs: listed, recent };
}

/** Bring a run's tabs to the front: expand its group, select its tab and focus its window. */
async function focusRun(conn) {
  const run = runs.get(conn);
  const tabId = run && [...run.tabs][0];
  if (tabId === undefined) return;
  if (run.groupId !== undefined) await chrome.tabGroups.update(run.groupId, { collapsed: false }).catch(() => {});
  const tab = await chrome.tabs.update(tabId, { active: true });
  await chrome.windows.update(tab.windowId, { focused: true });
}

chrome.runtime.onMessage.addListener((message, _sender, reply) => {
  if (message.type === 'runs') {
    overview().then(reply);
    return true;
  } else if (message.type === 'stop') {
    const conns = message.conn === undefined ? [...runs.keys()] : [message.conn];
    for (const conn of conns) stopRun(conn, 'popup');
    Promise.all(conns.map(conn => releaseRun(conn, 'popup'))).then(() => reply(true));
    return true;
  } else if (message.type === 'focus') {
    focusRun(message.conn).then(() => reply(true), () => reply(false));
    return true;
  } else if (message.type === 'label') {
    chrome.storage.local.set({ label: message.label }).then(() => {
      port?.disconnect();
      connect();
      reply(true);
    });
    return true;
  }
});

async function connect() {
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch {
    port = null;
    return;
  }
  port.onMessage.addListener(handle);
  port.onDisconnect.addListener(() => {
    port = null;
    for (const conn of [...runs.keys()]) releaseRun(conn, 'host_disconnected');
  });
  send({ type: 'hostHello', protocol: PROTOCOL, ...(await identity()) });
}

// The worker sleeps when idle; the alarm wakes it to reconnect to a host that is not running.
chrome.alarms.create('fastbrowse-host', { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(() => {
  if (!port) connect();
});
// Chrome starts a worker at launch only for an extension listening for onStartup, and alarms need not survive a
// restart, so without these the extension stayed unconnected after Chrome restarted until its popup was opened.
chrome.runtime.onStartup.addListener(() => {
  if (!port) connect();
});
chrome.runtime.onInstalled.addListener(() => {
  if (!port) connect();
});
connect();
