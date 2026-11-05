// The popup: the runs working in this browser, each with a Stop button, and the name `--extension-browser` matches.

const list = document.getElementById('runs');
const label = document.getElementById('label');

async function render() {
  const runs = await chrome.runtime.sendMessage({ type: 'runs' });
  list.replaceChildren();
  if (!runs.length) {
    const empty = document.createElement('li');
    empty.className = 'empty';
    empty.textContent = 'No run is working in this browser.';
    list.append(empty);
    return;
  }
  for (const run of runs) {
    const item = document.createElement('li');
    const task = document.createElement('span');
    task.className = 'task';
    task.textContent = `${run.task} (${run.tabs} tab${run.tabs === 1 ? '' : 's'})`;
    const stop = document.createElement('button');
    stop.textContent = 'Stop';
    stop.addEventListener('click', async () => {
      await chrome.runtime.sendMessage({ type: 'stop', conn: run.conn });
      render();
    });
    item.append(task, stop);
    list.append(item);
  }
}

chrome.storage.local.get('label').then(({ label: saved }) => {
  label.value = saved || '';
});
label.addEventListener('change', () => chrome.runtime.sendMessage({ type: 'label', label: label.value.trim() }));
render();
