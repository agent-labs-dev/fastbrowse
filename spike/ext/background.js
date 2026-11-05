// Relays CDP between the native host and chrome.debugger. Throwaway spike for #793.
let port;
const groups = new Map(); // run -> groupId
function send(msg) { port.postMessage(msg); }
async function handle(m) {
  const reply = (body) => send({ id: m.id, ...body });
  try {
    if (m.op === "hello") return reply({ result: { protocol: 1, extensionId: chrome.runtime.id } });
    if (m.op === "newTab") {
      const tab = await chrome.tabs.create({ url: m.url, active: false });
      const groupId = await chrome.tabs.group({ tabIds: [tab.id] });
      await chrome.tabGroups.update(groupId, { title: `fastbrowse: ${m.run}`, color: "blue" });
      groups.set(m.run, groupId);
      return reply({ result: { tabId: tab.id, groupId } });
    }
    if (m.op === "attach") { await chrome.debugger.attach({ tabId: m.tabId }, "1.3"); return reply({ result: {} }); }
    if (m.op === "detach") { await chrome.debugger.detach({ tabId: m.tabId }); return reply({ result: {} }); }
    if (m.op === "closeTab") { await chrome.tabs.remove(m.tabId); return reply({ result: {} }); }
    if (m.op === "cdp") {
      const target = m.sessionId ? { tabId: m.tabId, sessionId: m.sessionId } : { tabId: m.tabId };
      const result = await chrome.debugger.sendCommand(target, m.method, m.params || {});
      return reply({ result: result ?? {} });
    }
    reply({ error: `unknown op ${m.op}` });
  } catch (e) { reply({ error: String(e && e.message || e) }); }
}
chrome.debugger.onEvent.addListener((source, method, params) => {
  if (method === "Page.loadEventFired" || method === "Runtime.consoleAPICalled")
    send({ event: method, tabId: source.tabId, params });
});
chrome.debugger.onDetach.addListener((source, reason) => send({ event: "detached", tabId: source.tabId, reason }));
function connect() {
  port = chrome.runtime.connectNative("dev.fastbrowse.spike");
  port.onMessage.addListener(handle);
  port.onDisconnect.addListener(() => setTimeout(connect, 1000));
}
connect();
