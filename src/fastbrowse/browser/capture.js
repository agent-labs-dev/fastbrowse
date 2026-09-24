// Complete structured text capture of one frame, as ordered blocks with heading paths.
// Unlike snapshot.js (bounded, for Jev's action choice) this reads the whole rendered document, on screen or not,
// so the reading path and answer-evidence quotes see everything a user could scroll to.
// Adjacent inline content (text, <strong>, <a>, <span>...) is one block, so a value inside markup stays with its label.
(() => {
  if (!document.body) return null;
  const HEADINGS = { H1: 1, H2: 2, H3: 3, H4: 4, H5: 5, H6: 6 };
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME']);
  const blocks = [];
  let path = [];
  let framePath = null;
  let sourcePath = '';
  let inaccessible = 0;
  const registry = window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  const identity = e => {
    if (!registry.ids.has(e)) registry.ids.set(e, registry.next++);
    return registry.ids.get(e);
  };

  const clean = s => s.replace(/[ \t\r\f\v]+/g, ' ').replace(/ *\n */g, '\n').replace(/\n{3,}/g, '\n\n').trim();
  const textOf = e => clean(e.innerText ?? e.textContent ?? '');
  const hidden = e => e.hidden || getComputedStyle(e).display === 'none';
  const isBlock = e => {
    const display = getComputedStyle(e).display;
    return !(display.startsWith('inline') || display === 'contents') || e.tagName === 'BR';
  };
  const hrefOf = a => {
    const u = new URL(a.href, location.href);
    return u.origin === location.origin ? u.pathname + u.search : u.href;
  };
  const push = (kind, text, extra = {}) => {
    if (text) blocks.push({ kind, text, heading_path: [...path], frame_path: framePath,
      source_path: sourcePath, ...extra });
  };

  // A cell can say what it says with an image or an icon alone: a flag marking the winner, a tick for "yes".
  // Its text is then empty and the table reads as if the cell were blank, so name what is drawn there: the
  // image's alt text or label, or failing that the icon font's class, which is how those fonts name a glyph.
  const ICON_CLASS = /^(?:glyphicon|fa|fas|far|bi|mdi|icon|ti|la)-(.+)$/;
  // Font sizes, weights and effects share the prefix without naming a glyph.
  const ICON_STYLE = /^(?:solid|regular|light|thin|duotone|brands|xs|sm|lg|xl|\d+x|fw|spin|pulse|border|inverse|stack.*|pull-.*|rotate-.*|flip-.*)$/;
  const drawn = c => {
    for (const e of c.querySelectorAll('img,svg,i,span,[role="img"]')) {
      if (hidden(e)) continue;
      const named = e.getAttribute('alt') || e.getAttribute('aria-label') || e.getAttribute('title')
        || e.querySelector?.(':scope > title')?.textContent;
      if (named?.trim()) return `[${clean(named)}]`;
      for (const token of e.classList) {
        const glyph = ICON_CLASS.exec(token);
        if (glyph && !ICON_STYLE.test(glyph[1])) return `[${glyph[1].replace(/-/g, ' ')} icon]`;
      }
    }
    return '';
  };
  const cellText = c => textOf(c) || drawn(c);

  const renderTable = table => {
    // A table filter hides what it excludes with `hidden`, display or visibility (`collapse` is the one CSS made
    // for rows), and a column toggle hides cells; read as rows, a filtered table answered from rows not shown.
    // Visibility inherits, so the row's own style covers a collapsed section; display does not, so its section
    // is checked too. A cell hidden by visibility still holds its column, so it stays, blank, and the cells
    // after it keep their headers.
    const rows = [...table.querySelectorAll(':scope > tr, :scope > thead > tr, :scope > tbody > tr, :scope > tfoot > tr')]
      .filter(row => !hidden(row) && getComputedStyle(row).visibility === 'visible' && !hidden(row.parentElement))
      .map(row => ({ row, cells: [...row.querySelectorAll(':scope > th, :scope > td')].filter(c => !hidden(c)) }))
      .filter(({ cells }) => cells.length);
    if (!rows.length) return [];
    let headers = rows.filter(({ row }) => row.parentElement.tagName === 'THEAD');
    if (!headers.length && rows[0].cells.every(c => c.tagName === 'TH')) headers = [rows[0]];
    const line = ({ cells }) => '| ' + cells.map(c => cellText(c).replace(/\|/g, '\\|')).join(' | ') + ' |';
    const prefix = headers.map(line);
    if (headers.length) prefix.push('| ' + headers[0].cells.map(() => '---').join(' | ') + ' |');
    const data = rows.filter(row => !headers.includes(row));
    return data.length ? data.map(row => [...prefix, line(row)].join('\n')) : [prefix.join('\n')];
  };

  function flush(run) {
    if (!run.length) return;
    const elements = run.filter(n => n.nodeType === 1);
    const text = clean(run.map(n => (n.nodeType === 3 ? n.textContent : n.innerText ?? '')).join(''));
    const onlyLink = elements.length === 1 && elements[0].tagName === 'A' && elements[0].href
      && clean(elements[0].innerText ?? '') === text;
    if (onlyLink) push('link', text, { href: hrefOf(elements[0]) });
    else push('paragraph', text);
    run.length = 0;
  }

  function leaf(el) {
    return ![...el.children].some(c => !SKIP.has(c.tagName) && !hidden(c) && isBlock(c));
  }

  // A classless div repeated three times is usually page layout; a record is a styled unit or a semantic item.
  function repeated(el) {
    if (!el.classList.length && el.tagName !== 'ARTICLE' && el.tagName !== 'LI') return false;
    return [...(el.parentNode?.children ?? [])].filter(sibling => sibling.tagName === el.tagName
      && sibling.classList.length === el.classList.length
      && [...el.classList].every(token => sibling.classList.contains(token))).length >= 3;
  }

  const records = new WeakMap();
  function recordText(el) {
    if (records.has(el)) return records.get(el);
    records.set(el, null);
    if (!repeated(el) || el.shadowRoot || el.querySelector('table,iframe,frame')) return null;
    const descendants = [...el.querySelectorAll('*')];
    if (descendants.some(c => c.shadowRoot) || descendants.filter(c => HEADINGS[c.tagName]).length > 1) return null;
    if (!descendants.some(c => !SKIP.has(c.tagName) && !hidden(c) && isBlock(c))) return null;
    const text = textOf(el);
    if (!text || text.length > 1500) return null;
    // A repeated wrapper around another list is a group, so keep the inner records separately citable.
    if (descendants.some(c => (c.tagName === 'LI' && leaf(c)) || recordText(c) !== null)) return null;
    records.set(el, text);
    return text;
  }

  function walk(el) {
    const run = [];
    for (const node of el.childNodes) {
      if (node.nodeType === 3) {
        run.push(node);
        continue;
      }
      if (node.nodeType !== 1 || SKIP.has(node.tagName) || hidden(node)) continue;
      if (!isBlock(node) && recordText(node) === null) {
        run.push(node);
        continue;
      }
      flush(run);
      block(node);
    }
    flush(run);
  }

  function block(el) {
    const level = HEADINGS[el.tagName];
    if (level) {
      const text = textOf(el);
      while (path.length >= level) path.pop();
      if (text) {
        push('heading', text);
        path.push(text);
      }
      return;
    }
    if (el.tagName === 'TABLE') {
      for (const text of renderTable(el)) push('table', text);
      return;
    }
    if (el.tagName === 'PRE') return push('code', textOf(el));
    const record = recordText(el);
    if ((el.tagName === 'LI' && leaf(el)) || record !== null) {
      const links = el.querySelectorAll('a[href]');
      return push(el.tagName === 'LI' ? 'list_item' : 'record', record ?? textOf(el),
        links.length === 1 ? { href: hrefOf(links[0]) } : {});
    }
    walk(el);
  }

  function scope(root, frame, source) {
    const previous = [path, framePath, sourcePath];
    path = [];
    framePath = frame;
    sourcePath = source;
    walk(root);
    for (const e of root.querySelectorAll('*')) {
      if (e.shadowRoot) scope(e.shadowRoot, frame, `${source}/shadow:${identity(e)}`);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        let inner = null;
        try { inner = e.contentDocument; } catch { inner = null; }
        if (inner?.body) {
          const child = `${frame || 'root'}/${identity(e)}`;
          scope(inner.body, child, child);
        } else inaccessible++;
      }
    }
    [path, framePath, sourcePath] = previous;
  }

  scope(document.body, null, '');
  return { url: location.href, title: document.title, blocks, inaccessible_frames: inaccessible };
})()
