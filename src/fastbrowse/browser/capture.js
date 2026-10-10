// Complete structured text capture of one frame, as ordered blocks with heading paths.
// Unlike snapshot.js (bounded, for Jev's action choice) this reads the whole rendered document, on screen or not,
// so the reading path and answer-evidence quotes see everything a user could scroll to.
// Adjacent inline content (text, <strong>, <a>, <span>...) is one block, so a value inside markup stays with its label.
// biome-ignore format: Python embeds this function inside an expression, where a semicolon is invalid.
((allowedOrigins = null) => {
  if (!document.body) return null;
  const HEADINGS = { H1: 1, H2: 2, H3: 3, H4: 4, H5: 5, H6: 6 };
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME']);
  const blocks = [];
  let path = [];
  let framePath = null;
  let sourcePath = '';
  let inaccessible = 0;
  let visibleH1 = false;
  let controlObservations = 0;
  let imageObservations = 0;
  let imagesOmitted = false;
  let controlsOmitted = false;
  const imageSource = source => {
    if (!source) return source;
    if (source.startsWith('data:')) {
      const comma = source.indexOf(',');
      return `${source.slice(0, comma < 0 ? 128 : Math.min(comma, 128))},[payload omitted]`;
    }
    if (source.startsWith('blob:')) return 'blob:[identifier omitted]';
    return source.length > 1024 ? `${source.slice(0, 1024)}[truncated]` : source;
  };
  const registry = (window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 });
  const granted = doc => {
    if (allowedOrigins === null) return true;
    const frame = doc.defaultView.frameElement;
    if (frame) {
      if (frame.hasAttribute('srcdoc') || !frame.getAttribute('src')) return false;
      const source = new URL(frame.src);
      if (!['http:', 'https:'].includes(source.protocol) || !allowedOrigins.includes(source.origin)) return false;
    }
    const url = new URL(doc.URL);
    return ['http:', 'https:'].includes(url.protocol) && allowedOrigins.includes(url.origin);
  };
  const identity = e => {
    if (!registry.ids.has(e)) registry.ids.set(e, registry.next++);
    return registry.ids.get(e);
  };

  const clean = s =>
    s
      .replace(/[ \t\r\f\v]+/g, ' ')
      .replace(/ *\n */g, '\n')
      .replace(/\n{3,}/g, '\n\n')
      .trim();
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
  const controlOf = nodes => {
    let found = null;
    for (const node of nodes) {
      if (!clean(node.nodeType === 3 ? node.textContent : (node.innerText ?? node.textContent ?? ''))) continue;
      let host = node.nodeType === 1 ? node : node.parentElement;
      while (host && !registry.hoverContexts?.has(host)) host = host.parentElement;
      if (!host?.isConnected || !host.matches(':hover')) return null;
      const binding = registry.hoverContexts.get(host);
      const siblings = [...(host.parentNode?.children || [])];
      const labelAttributes = [host.getAttribute('role'), host.getAttribute('aria-label'),
        host.getAttribute('title'), host.querySelector('img[alt]')?.getAttribute('alt') || null];
      if (host.innerText !== binding.text ||
          labelAttributes.some((value, index) => value !== binding.labelAttributes[index])) return null;
      // A reordered or replaced sibling set invalidates the observed ordinal before the quote is taken.
      if (host.parentNode !== binding.parent || siblings.length !== binding.siblings.length ||
          siblings.some((sibling, index) => sibling !== binding.siblings[index])) return null;
      if (found && found !== host) return null;
      found = host;
    }
    return found ? registry.hoverContexts.get(found).control : null;
  };
  const listIds = new WeakMap();
  let nextListId = 0;
  const listContext = nodes => {
    const lists = new Set();
    for (const node of nodes) {
      let element = node.nodeType === 1 ? node : node.parentElement;
      let list = null;
      for (let parent = element; parent; parent = parent.parentElement) {
        if (parent.matches('nav,[role="navigation"]')) return {};
        if (parent.matches('ul,ol,[role="list"]')) list = parent;
      }
      if (!list) {
        const nested = [...(element?.querySelectorAll('ul,ol,[role="list"]') ?? [])];
        const outer = nested.filter(candidate => !nested.some(other => other !== candidate && other.contains(candidate)));
        if (outer.length === 1) list = outer[0];
      }
      if (list) lists.add(list);
    }
    if (lists.size !== 1) return {};
    const list = [...lists][0];
    if (!listIds.has(list)) listIds.set(list, String(nextListId++));
    const members = [...list.querySelectorAll('li,[role="listitem"]')].filter(member => {
      for (let parent = member; parent && parent !== list; parent = parent.parentElement) {
        if (hidden(parent)) return false;
      }
      return !!textOf(member);
    });
    return { list_id: listIds.get(list), list_count: members.length };
  };
  const push = (kind, text, extra = {}, nodes = []) => {
    if (text)
      blocks.push({ kind, text, heading_path: [...path], frame_path: framePath, source_path: sourcePath, control_context: controlOf(nodes), ...listContext(nodes), ...extra });
  };

  // A cell can say what it says with an image or an icon alone: a flag marking the winner, a tick for "yes".
  // Its text is then empty and the table reads as if the cell were blank, so name what is drawn there: the
  // image's alt text or label, or failing that the icon font's class, which is how those fonts name a glyph.
  const ICON_CLASS = /^(?:glyphicon|fa|fas|far|bi|mdi|icon|ti|la)-(.+)$/;
  // Font sizes, weights and effects share the prefix without naming a glyph.
  const ICON_STYLE =
    /^(?:solid|regular|light|thin|duotone|brands|xs|sm|lg|xl|\d+x|fw|spin|pulse|border|inverse|stack.*|pull-.*|rotate-.*|flip-.*)$/;
  const drawn = c => {
    for (const e of c.querySelectorAll('img,svg,i,span,[role="img"]')) {
      if (hidden(e)) continue;
      const named =
        e.getAttribute('alt') ||
        e.getAttribute('aria-label') ||
        e.getAttribute('title') ||
        e.querySelector?.(':scope > title')?.textContent;
      if (named?.trim()) return `[${clean(named)}]`;
      for (const token of e.classList) {
        const glyph = ICON_CLASS.exec(token);
        if (glyph && !ICON_STYLE.test(glyph[1])) return `[${glyph[1].replace(/-/g, ' ')} icon]`;
      }
    }
    return '';
  };
  // A rating widget states its value only in markup a reader cannot see: a `star-rating` class carrying a
  // One..Five word token, or an accessible label the icons alone do not carry. Counting icons never works,
  // since a site colours a fixed row of them by class instead of drawing one per star (books.toscrape.com:
  // `<p class="star-rating Five">`, five identical `<i class="icon-star">` children whatever the rating).
  // Only these two explicit sources are trusted; a class that merely mentions "star" or an icon glyph is not
  // read as a rating, and a hidden widget states nothing a reader could see either.
  const RATING_WORD = { one: 'One', two: 'Two', three: 'Three', four: 'Four', five: 'Five' };
  const RATING_LABEL = /\b[1-5]\s*(?:\/|out of)\s*5\b|\b[1-5]\s*stars?\b/i;
  const ratingOf = e => {
    if (hidden(e) || getComputedStyle(e).visibility === 'hidden') return '';
    const classes = [...e.classList].map(c => c.toLowerCase());
    if (classes.includes('star-rating')) {
      for (const token of classes) if (RATING_WORD[token]) return `${RATING_WORD[token]} stars`;
    }
    const label = e.getAttribute('aria-label') || e.getAttribute('title');
    return label && RATING_LABEL.test(label) ? clean(label) : '';
  };
  // Checked on the element itself, then any descendant a class or role names as the widget, since a
  // role="img" label commonly sits one level below the card holding the title and price. A widget hidden
  // by an ancestor (rather than itself) states nothing a reader could see either, so ratingOf's own
  // `hidden` check is not enough: walk up to the search root and reject any candidate under a hidden one.
  const ratingIn = c =>
    ratingOf(c) ||
    [...c.querySelectorAll('[class*="star" i], [class*="rating" i], [role="img"]')]
      .filter(e => {
        for (let a = e.parentElement; a && a !== c; a = a.parentElement) if (hidden(a)) return false;
        return true;
      })
      .map(ratingOf)
      .find(Boolean) ||
    '';
  const withRating = (text, c) => {
    const rating = ratingIn(c);
    if (!rating || text.includes(rating)) return text;
    return text ? `${text} (${rating})` : rating;
  };
  const cellText = c => withRating(textOf(c) || drawn(c), c);

  // What a form holds is in its controls' values, which no text node carries: a task that asked for dates to be
  // chosen was refused as unanswered because "the dates chosen" had nothing to quote. A filled control reads as
  // "Label: value" after the text around it. Password and agent-typed secret values never leave the page.
  const CONTROLS = new Set(['INPUT', 'SELECT', 'TEXTAREA']);
  const UNFILLED = new Set(['hidden', 'password', 'checkbox', 'radio', 'button', 'submit', 'reset', 'image', 'file']);
  const nameOf = (e, seen = new Set()) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    const root = e.getRootNode?.() ?? document;
    const byId = id => (root.getElementById ? root : document).getElementById(id);
    const referenced = (e.getAttribute('aria-labelledby') || '')
      .split(/\s+/)
      .filter(Boolean)
      .map(id => nameOf(byId(id), seen))
      .filter(Boolean)
      .join(' ');
    return clean(
      referenced ||
        e.getAttribute('aria-label') ||
        [...(e.labels || [])]
          .map(l => nameOf(l, seen))
          .filter(Boolean)
          .join(' ') ||
        (CONTROLS.has(e.tagName)
          ? ''
          : [...e.childNodes]
              .map(n =>
                n.nodeType === 3
                  ? n.textContent
                  : n.nodeType === 1 && !SKIP.has(n.tagName) && !CONTROLS.has(n.tagName)
                    ? nameOf(n, seen)
                    : '',
              )
              .join(' ')) ||
        e.getAttribute('title') ||
        e.getAttribute('placeholder') ||
        e.getAttribute('name') ||
        '',
    );
  };
  const fieldValue = e => {
    if (e.tagName === 'SELECT') return [...e.selectedOptions].map(o => clean(o.label || o.text)).join(', ');
    if (UNFILLED.has(e.type) || e.dataset?.fastbrowseSecret === '1') return '';
    return clean(e.value ?? '');
  };
  const fields = el =>
    (CONTROLS.has(el.tagName) ? [el] : [...el.querySelectorAll('input,select,textarea')])
      .filter(e => (e.checkVisibility ? e.checkVisibility({ visibilityProperty: true }) : !hidden(e)))
      .map(e => [nameOf(e), fieldValue(e)])
      .filter(([, value]) => value)
      .map(([name, value]) => (name ? `${name}: ${value}` : value));

  const renderTable = table => {
    // A table filter hides what it excludes with `hidden`, display or visibility (`collapse` is the one CSS made
    // for rows), and a column toggle hides cells; read as rows, a filtered table answered from rows not shown.
    // Visibility inherits, so the row's own style covers a collapsed section; display does not, so its section
    // is checked too. A cell hidden by visibility still holds its column, so it stays, blank, and the cells
    // after it keep their headers.
    const rows = [
      ...table.querySelectorAll(':scope > tr, :scope > thead > tr, :scope > tbody > tr, :scope > tfoot > tr'),
    ]
      .filter(row => !hidden(row) && getComputedStyle(row).visibility === 'visible' && !hidden(row.parentElement))
      .map(row => ({ row, cells: [...row.querySelectorAll(':scope > th, :scope > td')].filter(c => !hidden(c)) }))
      .filter(({ cells }) => cells.length);
    if (!rows.length) return [];
    let headers = rows.filter(({ row }) => row.parentElement.tagName === 'THEAD');
    if (!headers.length && rows[0].cells.every(c => c.tagName === 'TH')) headers = [rows[0]];
    const line = ({ cells }) => '| ' + cells.map(c => cellText(c).replace(/\|/g, '\\|')).join(' | ') + ' |';
    // Headerless comparison rows need leading cells for identity and separate source spans for counting.
    if (!headers.length) return rows.map((row, index) => (index ? [line(rows[0]), line(row)].join('\n') : line(row)));
    const prefix = headers.map(line);
    if (headers.length) prefix.push('| ' + headers[0].cells.map(() => '---').join(' | ') + ' |');
    const data = rows.filter(row => !headers.includes(row));
    return data.length ? data.map(row => [...prefix, line(row)].join('\n')) : [prefix.join('\n')];
  };

  function flush(run) {
    if (!run.length) return;
    const elements = run.filter(n => n.nodeType === 1);
    let text = clean(run.map(n => (n.nodeType === 3 ? n.textContent : (n.innerText ?? ''))).join(''));
    const onlyLink =
      elements.length === 1 &&
      elements[0].tagName === 'A' &&
      elements[0].href &&
      clean(elements[0].innerText ?? '') === text;
    for (const e of elements) text = withRating(text, e);
    if (onlyLink) push('link', text, { href: hrefOf(elements[0]) }, run);
    else push('paragraph', text, {}, run);
    for (const e of elements) for (const field of fields(e)) push('paragraph', field);
    run.length = 0;
  }

  function leaf(el) {
    return ![...el.children].some(c => !SKIP.has(c.tagName) && !hidden(c) && isBlock(c));
  }

  // A classless div repeated three times is usually page layout; a record is a styled unit or a semantic item.
  function repeated(el) {
    if (!el.classList.length && el.tagName !== 'ARTICLE' && el.tagName !== 'LI') return false;
    const siblings = [...(el.parentNode?.children ?? [])].filter(
        sibling =>
          sibling.tagName === el.tagName &&
          sibling.classList.length === el.classList.length &&
          [...el.classList].every(token => sibling.classList.contains(token)),
      );
    if (siblings.length >= 3) return true;
    // Linked cards can sit inside an unlabelled list wrapper, so distinct destinations establish the pair.
    if (siblings.length === 2 && el.tagName === 'A' && siblings.every(sibling => sibling.hasAttribute('href'))) {
      return new Set(siblings.map(sibling => sibling.href)).size === siblings.length;
    }
    // A styled pair can be a two-column layout, so require a heading outside both items to name their group.
    const section = el.parentElement;
    return (
      siblings.length === 2 &&
      [...(section?.querySelectorAll('h1,h2,h3,h4,h5,h6') ?? [])].some(heading => siblings.every(sibling => !sibling.contains(heading)))
    );
  }

  const records = new WeakMap();
  function recordText(el) {
    if (records.has(el)) return records.get(el);
    records.set(el, null);
    if (!repeated(el) || el.shadowRoot || el.querySelector('table,iframe,frame')) return null;
    const descendants = [...el.querySelectorAll('*')];
    if (descendants.some(c => c.shadowRoot) || descendants.filter(c => HEADINGS[c.tagName]).length > 1) return null;
    if (!descendants.some(c => !SKIP.has(c.tagName) && !hidden(c) && isBlock(c))) return null;
    const text = withRating(textOf(el), el);
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
      if (node.nodeType !== 1 || hidden(node)) continue;
      if (FRAMES.has(node.tagName)) {
        flush(run);
        enter(node, path);
        continue;
      }
      if (SKIP.has(node.tagName)) continue;
      if (!node.matches('nav,[role="navigation"]') && !isBlock(node) && recordText(node) === null) {
        run.push(node);
        continue;
      }
      flush(run);
      block(node);
    }
    flush(run);
  }

  function block(el) {
    if (el.matches('nav,[role="navigation"]')) {
      // Navigation help headings describe their menu, not product facts in the following main content.
      const surrounding = [...path];
      walk(el);
      path = surrounding;
      return;
    }
    const level = HEADINGS[el.tagName];
    if (level) {
      const text = textOf(el);
      while (path.length >= level) path.pop();
      if (text) {
        push('heading', text, {}, [el]);
        path.push(text);
      }
      return;
    }
    if (el.tagName === 'TABLE') {
      for (const text of renderTable(el)) push('table', text, {}, [el]);
      // A form laid out in a table: its cells render as text, which holds none of the controls' values.
      for (const field of fields(el)) push('paragraph', field);
      return;
    }
    if (el.tagName === 'PRE') return push('code', textOf(el), {}, [el]);
    const record = recordText(el);
    if ((el.tagName === 'LI' && leaf(el)) || record !== null) {
      const links = el.matches('a[href]') ? [el] : el.querySelectorAll('a[href]');
      push(
        el.tagName === 'LI' ? 'list_item' : 'record',
        record ?? withRating(textOf(el), el),
        links.length === 1 ? { href: hrefOf(links[0]) } : {},
        [el],
      );
      for (const field of fields(el)) push('paragraph', field);
      return;
    }
    if (CONTROLS.has(el.tagName)) {
      for (const field of fields(el)) push('paragraph', field);
      return;
    }
    walk(el);
  }

  // A same-origin frame is read where it stands, under the headings above it: read after the whole page, an
  // embedded subscription form's heading followed the footer, and the reader, asked for the heading the
  // frame shows, answered with the page's heading over the frame instead.
  const FRAMES = new Set(['IFRAME', 'FRAME']);
  const entered = new WeakSet();
  function enter(e, headings) {
    entered.add(e);
    let inner = null;
    try {
      inner = e.contentDocument;
    } catch {
      inner = null;
    }
    if (!inner?.body || !granted(inner)) {
      inaccessible++;
      return;
    }
    const child = `${framePath || 'root'}/${identity(e)}`;
    scope(inner.body, child, child, headings);
  }

  function scope(root, frame, source, headings = []) {
    const previous = [path, framePath, sourcePath];
    path = [...headings];
    framePath = frame;
    sourcePath = source;
    walk(root);
    path = [...headings];
    const visible = e => e.checkVisibility ? e.checkVisibility({ visibilityProperty: true }) : !hidden(e);
    for (const e of [...root.querySelectorAll('input,select,textarea,button,[role="button"]')].filter(visible)) {
      if (controlObservations >= 64) {
        if (!controlsOmitted) push('observation', 'Additional control state observations omitted.');
        controlsOmitted = true;
        break;
      }
      // Placeholders suggest input; they do not label a value or prove an empty named field.
      if (CONTROLS.has(e.tagName) && !e.labels?.length && !e.hasAttribute('aria-label') && !e.hasAttribute('aria-labelledby')) continue;
      const name = nameOf(e) || clean(e.innerText ?? '');
      if (!name || e.dataset?.fastbrowseSecret === '1') continue;
      if (CONTROLS.has(e.tagName) && !UNFILLED.has(e.type) && !fieldValue(e)) {
        push('observation', `${name.slice(0, 1024)}: [empty]`);
        controlObservations++;
      }
      if (controlObservations < 64 && e.matches(':disabled,[aria-disabled="true"]')) {
        push('observation', `${name.slice(0, 1024)}: disabled`);
        controlObservations++;
      }
    }
    // Images carry no innerText, so a text-only capture cannot distinguish their presence from absence.
    for (const image of [...root.querySelectorAll('img')].filter(visible)) {
      if (imageObservations++ >= 32) {
        if (!imagesOmitted) push('observation', 'Additional visible image metadata omitted.');
        imagesOmitted = true;
        break;
      }
      // Loading changes without a page edit, so captures report presence rather than transient decode state.
      const metadata = { alt: image.getAttribute('alt')?.slice(0, 1024) ?? null,
        src: imageSource(image.getAttribute('src') ?? image.getAttribute('srcset')) };
      push('observation', `Image element DOM metadata: ${JSON.stringify(metadata)}. Pixels are not described.`);
    }
    const h1 = [...root.querySelectorAll('h1,[role="heading"][aria-level="1"]')].filter(visible);
    visibleH1 ||= h1.length > 0;
    if (root === document.body) {
      const doc = document.documentElement;
      push('observation', `Observed viewport: Horizontal overflow: ${doc.scrollWidth > doc.clientWidth ? 'yes' : 'no'}`);
    }
    for (const e of root.querySelectorAll('*')) {
      if (e.shadowRoot) scope(e.shadowRoot, frame, `${source}/shadow:${identity(e)}`);
      // A frame the walk never reached (hidden, or inside a record or table read as one block) is still read.
      if (FRAMES.has(e.tagName) && !entered.has(e)) enter(e, []);
    }
    [path, framePath, sourcePath] = previous;
  }

  scope(document.body, null, '');
  if (!visibleH1) push('observation', 'Observed accessible DOM: Visible h1 headings: 0');
  return { url: location.href, title: document.title, blocks, inaccessible_frames: inaccessible, visible_h1: visibleH1 };
})
