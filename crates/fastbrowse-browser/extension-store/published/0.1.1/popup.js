// The popup: whether fastbrowse is connected, the runs working in this browser, the last few that ended, and the
// name `--extension-browser` matches.

const list = document.getElementById('runs');
const recentList = document.getElementById('recent');
const status = document.getElementById('status');
const stopAll = document.getElementById('stop-all');
const label = document.getElementById('label');
const saved = document.getElementById('saved');
const taker = document.getElementById('taker');

const ENDINGS = {
  ended: 'ended',
  popup: 'stopped',
  debugging_bar: 'cancelled',
  last_tab_closed: 'tabs closed',
  other_extension: 'tab taken',
  host_disconnected: 'disconnected',
};

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function elapsed(since) {
  const seconds = Math.max(0, Math.round((Date.now() - since) / 1000));
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

function runItem(run) {
  const item = element('li', 'run');
  item.title = 'Show this run\'s tabs';
  const text = element('div', 'text');
  const where = run.title || run.host;
  const meta = [`${run.tabs} tab${run.tabs === 1 ? '' : 's'}`, elapsed(run.started), where].filter(Boolean);
  text.append(element('div', 'task', run.task), element('div', 'meta', meta.join(' · ')));
  const stop = element('button', '', 'Stop');
  stop.addEventListener('click', async event => {
    event.stopPropagation();
    stop.disabled = true;
    await chrome.runtime.sendMessage({ type: 'stop', conn: run.conn });
    render();
  });
  item.addEventListener('click', () => chrome.runtime.sendMessage({ type: 'focus', conn: run.conn }));
  item.append(text, stop);
  return item;
}

async function render() {
  const { connected, browserId, runs, recent } = await chrome.runtime.sendMessage({ type: 'runs' });
  status.className = `status ${connected ? 'on' : 'off'}`;
  status.textContent = connected ? 'Connected' : 'Not connected';
  document.getElementById('offline').hidden = connected;
  document.getElementById('browser-id').textContent = `ID ${browserId}`;
  stopAll.hidden = runs.length < 2;

  list.replaceChildren();
  if (!runs.length) {
    const empty = element('li', 'empty');
    if (connected) {
      empty.append('Ready for a task. Start one in your terminal:', element('code', '', 'fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser'));
    } else {
      empty.append('Connect Chrome to fastbrowse to start a run.');
    }
    list.append(empty);
  }
  for (const run of runs) list.append(runItem(run));

  document.getElementById('recent-section').hidden = !recent.length;
  recentList.replaceChildren();
  for (const run of recent) {
    const item = element('li');
    const bad = run.ending === 'other_extension' || run.ending === 'host_disconnected';
    item.append(element('span', 'task', run.task), element('span', `ending${bad ? ' bad' : ''}`, ENDINGS[run.ending] || run.ending));
    recentList.append(item);
  }
  // The latest run another extension took a tab from, linked to that extension's page when fastbrowse saw which.
  const taken = recent.find(run => run.ending === 'other_extension');
  document.getElementById('conflict').hidden = !taken;
  if (taken) {
    const id = taken.taker?.replace('chrome-extension://', '');
    taker.firstChild.textContent = id ? `chrome://extensions/?id=${id}` : 'chrome://extensions';
  }
}

// A page cannot link to chrome:// itself, so the extension opens it.
taker.addEventListener('click', event => {
  event.preventDefault();
  chrome.tabs.create({ url: taker.textContent });
});

stopAll.addEventListener('click', async () => {
  stopAll.disabled = true;
  await chrome.runtime.sendMessage({ type: 'stop' });
  stopAll.disabled = false;
  render();
});

chrome.storage.local.get('label').then(({ label: value }) => {
  label.value = value || '';
});
label.addEventListener('change', async () => {
  await chrome.runtime.sendMessage({ type: 'label', label: label.value.trim() });
  saved.classList.add('show');
  setTimeout(() => saved.classList.remove('show'), 1500);
});

render();
// Elapsed times and page titles move while the popup is open.
setInterval(render, 1000);
