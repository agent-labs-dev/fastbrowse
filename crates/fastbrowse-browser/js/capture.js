// Complete structured text capture of one frame, as ordered blocks with heading paths.
// Unlike snapshot.js (the viewport, for Jev's action choice) this reads the whole rendered document, on screen or not,
// so the reading path and answer-evidence quotes see everything a user could scroll to.
// Adjacent inline content (text, <strong>, <a>, <span>...) is one block, so a value inside markup stays with its label.
// A dated element (a `time`, or a custom element such as `relative-time`) reads as its shown text followed by its
// `datetime` in brackets, because the exact date a relative time such as "3 weeks ago" stands for is otherwise lost.
// biome-ignore format: CdpPage embeds this function inside an expression, where a semicolon is invalid.
((allowedOrigins = null) => {
  if (!document.body) return null;
  const HEADINGS = { H1: 1, H2: 2, H3: 3, H4: 4, H5: 5, H6: 6 };
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME']);
  const blocks = [];
  let path = [];
  // The headings a frame stands under: its own headings rank among themselves, so its h1 cannot end them.
  let floor = 0;
  let framePath = null;
  let sourcePath = '';
  let inaccessible = 0;
  const imageSource = source => {
    if (!source) return source;
    if (source.startsWith('data:')) {
      const comma = source.indexOf(',');
      return `${source.slice(0, comma < 0 ? 128 : Math.min(comma, 128))},[payload omitted]`;
    }
    if (source.startsWith('blob:')) return 'blob:[identifier omitted]';
    return source;
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
  const hidden = e => e.hidden || getComputedStyle(e).display === 'none';
  const visible = e => (e.checkVisibility ? e.checkVisibility({ visibilityProperty: true }) : !hidden(e));
  // The `datetime` of `ins` and `del` dates an edit, not the text, so only a `time` or a custom element is dated.
  const datable = e => e.localName === 'time' || e.localName.includes('-');
  const stampOf = e => (datable(e) && visible(e) ? (e.getAttribute('datetime') ?? '').trim() : '');
  // A host that draws its text in its own shadow root, as GitHub's relative-time does, has an empty innerText.
  const shownOf = e => {
    const own = clean(e.innerText ?? '');
    if (own || !e.shadowRoot) return own;
    const parts = [...e.shadowRoot.childNodes].map(n =>
      n.nodeType === 3 ? n.textContent : n.nodeType === 1 && !SKIP.has(n.tagName) ? (n.innerText ?? '') : '',
    );
    return clean(parts.join(''));
  };
  // `text` as `e` reads it, with each dated element's stamp written after the text it shows, in document order.
  const dated = (e, text) => {
    if (!datable(e) && !e.querySelector('[datetime]')) return text;
    let cursor = 0;
    for (const d of [...(datable(e) ? [e] : []), ...e.querySelectorAll('[datetime]')]) {
      const stamp = stampOf(d);
      const shown = stamp ? shownOf(d) : '';
      if (!shown) continue;
      if (d === e && !clean(text)) return shown.includes(stamp) ? shown : `${shown} (${stamp})`;
      const at = text.indexOf(shown, cursor);
      if (at < 0) continue;
      cursor = at + shown.length;
      if (shown.includes(stamp)) continue;
      text = `${text.slice(0, cursor)} (${stamp})${text.slice(cursor)}`;
      cursor += stamp.length + 3;
    }
    return text;
  };
  // A footnote reference is a pointer to a note elsewhere on the page, not part of the text it marks: read inline, an
  // infobox cell's `41,798,407[3]` was answered as the population, marker and all (#862). It is told by structure: a
  // link the page names a note reference (`doc-noteref`), or an in-page link whose target links back to exactly that
  // link, as Wikipedia, GitHub and Pandoc footnotes do. A table of contents or a "back to top" link is never answered
  // by a link to itself. The note itself stays, where the page lists it.
  const IN_PAGE = 'a[href^="#"],[role~="doc-noteref"]';
  const fragmentTarget = (root, href) => {
    if (!href?.startsWith('#') || href.length < 2) return null;
    let id = href.slice(1);
    try {
      id = decodeURIComponent(id);
    } catch {
      // An id that is not valid percent-encoding is looked up as written, as a browser does.
    }
    return (root.getElementById ? root : root.ownerDocument ?? document).getElementById(id);
  };
  const noteReferences = new WeakMap();
  const noteReference = e => {
    if (noteReferences.has(e)) return noteReferences.get(e);
    let found = (e.getAttribute('role') ?? '').split(/\s+/).includes('doc-noteref');
    const root = e.getRootNode?.() ?? document;
    const note = found ? null : fragmentTarget(root, e.getAttribute('href'));
    if (note && !note.contains(e)) {
      const marker = clean(e.innerText ?? '');
      // The back link may point at the reference's wrapper, such as Wikipedia's `sup`, which shows the link alone.
      found = [...note.querySelectorAll('a[href^="#"]')].some(back => {
        const target = fragmentTarget(root, back.getAttribute('href'));
        return target && target.contains(e) && clean(target.innerText ?? '') === marker;
      });
    }
    noteReferences.set(e, found);
    return found;
  };
  // `text` as `e` reads it, without the text of each footnote reference in it, removed in document order.
  const unnoted = (e, text) => {
    const references = [...(e.matches(IN_PAGE) ? [e] : []), ...e.querySelectorAll(IN_PAGE)].filter(noteReference);
    let cursor = 0;
    for (const reference of references) {
      const shown = clean(reference.innerText ?? '');
      const at = shown ? text.indexOf(shown, cursor) : -1;
      if (at < 0) continue;
      text = text.slice(0, at) + text.slice(at + shown.length);
      cursor = at;
    }
    return text;
  };
  const textOf = e => clean(dated(e, unnoted(e, e.innerText ?? e.textContent ?? '')));
  const isBlock = e => {
    const display = getComputedStyle(e).display;
    return !(display.startsWith('inline') || display === 'contents') || e.tagName === 'BR';
  };
  const hrefOf = a => {
    const u = new URL(a.href, location.href);
    return u.origin === location.origin ? u.pathname + u.search : u.href;
  };
  // A link's text reads as one with the words around it and says nothing of where it leads: a product name linked
  // inside its list item was taken with the item's price as the name, and asked for a description's hyperlinks, a run
  // had their texts and no address to give. Each link is kept with its block as its text and address, which code
  // places in the block's text. A footnote reference is no part of the text it marks, and a script link leads nowhere.
  const linksIn = elements =>
    elements
      .flatMap(e => [...(e.matches('a[href]') ? [e] : []), ...e.querySelectorAll('a[href]')])
      // An SVG link's `href` is an animated value, not an address.
      .filter(a => typeof a.href === 'string' && !a.href.startsWith('javascript:') && !(a.matches(IN_PAGE) && noteReference(a)))
      .map(a => ({ text: textOf(a), href: a.href }))
      .filter(link => link.text);
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
  const push = (kind, text, extra = {}, nodes = []) => {
    if (text)
      blocks.push({ kind, text, heading_path: [...path], frame_path: framePath, source_path: sourcePath, control_context: controlOf(nodes), ...extra });
  };

  // A cell can say what it says with an image or an icon alone: a flag marking the winner, a tick for "yes".
  // Its text is then empty and the table reads as if the cell were blank, so name what is drawn there: the
  // image's alt text or label, or failing that the icon font's class, which is how those fonts name a glyph.
  const ICON_CLASS =
    /^(?:icon-tabler|tabler-icon|lucide|glyphicon|fa|fas|far|bi|mdi|icon|ti|la|ri|ion|feather)-([a-z][a-z0-9-]*)$/;
  // Sizes, weights and effects share an icon library's prefix without naming a glyph, as does the library itself.
  const ICON_STYLE =
    /^(?:icon|tabler|solid|regular|light|thin|duotone|brands|xs|sm|lg|xl|\d+x|fw|spin|pulse|border|inverse|stack.*|pull-.*|rotate-.*|flip-.*)$/;
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
  const cellText = c => textOf(c) || drawn(c);

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
      .filter(visible)
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
    const divider = headers.length ? '| ' + headers[0].cells.map(() => '---').join(' | ') + ' |' : '';
    // A row block is its text with the links of the rows it writes, in the order it writes them.
    const of = (written, headed) => ({
      text: [...(headed ? [...headers.map(line), divider] : []), ...written.map(line)].join('\n'),
      links: linksIn([...(headed ? headers : []), ...written].flatMap(row => row.cells)),
    });
    // Headerless comparison rows need leading cells for identity and separate source spans for counting.
    if (!headers.length) return rows.map((row, index) => of(index ? [rows[0], row] : [row], false));
    const data = rows.filter(row => !headers.includes(row));
    return data.length ? data.map(row => of([row], true)) : [of([], true)];
  };

  function flush(run) {
    if (!run.length) return;
    const elements = run.filter(n => n.nodeType === 1);
    let text = clean(run.map(n => (n.nodeType === 3 ? n.textContent : dated(n, unnoted(n, n.innerText ?? '')))).join(''));
    // A host's own text read in its shadow root is a block apart from its row, which may have read it without it.
    // A frame's nodes come from another realm, so the root is told by its node type rather than `instanceof`.
    const host = run[0].parentNode?.nodeType === 11 ? run[0].parentNode.host : null;
    const stamp = host ? stampOf(host) : '';
    if (text && stamp && !text.includes(stamp)) text = `${text} (${stamp})`;
    const onlyLink =
      elements.length === 1 && elements[0].tagName === 'A' && elements[0].href && textOf(elements[0]) === text;
    const links = linksIn(elements);
    if (onlyLink) push('link', text, { href: hrefOf(elements[0]), links }, run);
    else push('paragraph', text, { links }, run);
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

  // A record can state a value only by drawing it: a row of identical icons a class colours has no text, and a capture
  // of the text reads every record as alike. The page's own label or class names it, verbatim, for the reader to judge.
  const LEAVES_OF_THEIR_OWN = 'a,button,img,input,select,textarea,svg';
  function drawnFacts(descendants) {
    const facts = [];
    for (const e of descendants) {
      if (!e.children.length || hidden(e) || textOf(e)) continue;
      if (e.matches(LEAVES_OF_THEIR_OWN) || e.querySelector(LEAVES_OF_THEIR_OWN)) continue;
      if (descendants.includes(e.parentElement) && !textOf(e.parentElement)) continue;
      const name = e.getAttribute('aria-label') || e.getAttribute('title') || [...e.classList].join(' ');
      if (clean(name)) facts.push(`[${clean(name)}]`);
    }
    return facts;
  }

  const records = new WeakMap();
  function recordText(el) {
    if (records.has(el)) return records.get(el);
    records.set(el, null);
    if (!repeated(el) || el.shadowRoot || el.querySelector('table,iframe,frame')) return null;
    const descendants = [...el.querySelectorAll('*')];
    if (descendants.some(c => c.shadowRoot) || descendants.filter(c => HEADINGS[c.tagName]).length > 1) return null;
    if (!descendants.some(c => !SKIP.has(c.tagName) && !hidden(c) && isBlock(c))) return null;
    const text = [textOf(el), ...drawnFacts(descendants)].filter(Boolean).join(' ');
    if (!text) return null;
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
      while (path.length > floor && path.length - floor >= level) path.pop();
      if (text) {
        push('heading', text, { links: linksIn([el]) }, [el]);
        path.push(text);
      }
      return;
    }
    if (el.tagName === 'TABLE') {
      for (const { text, links } of renderTable(el)) push('table', text, { links }, [el]);
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
        record ?? textOf(el),
        { ...(links.length === 1 ? { href: hrefOf(links[0]) } : {}), links: linksIn([el]) },
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
    const previous = [path, floor, framePath, sourcePath];
    path = [...headings];
    floor = headings.length;
    framePath = frame;
    sourcePath = source;
    walk(root);
    path = [...headings];
    for (const e of [...root.querySelectorAll('input,select,textarea,button,[role="button"]')].filter(visible)) {
      // Placeholders suggest input; they do not label a value or prove an empty named field.
      if (CONTROLS.has(e.tagName) && !e.labels?.length && !e.hasAttribute('aria-label') && !e.hasAttribute('aria-labelledby')) continue;
      const name = nameOf(e) || clean(e.innerText ?? '');
      if (!name || e.dataset?.fastbrowseSecret === '1') continue;
      if (CONTROLS.has(e.tagName) && !UNFILLED.has(e.type) && !fieldValue(e)) push('observation', `${name}: [empty]`);
      if (e.matches(':disabled,[aria-disabled="true"]')) push('observation', `${name}: disabled`);
    }
    // Which option is chosen is element state no text node carries, and a page often shows what follows from it in
    // place: a checkout re-rendered one order total per shipping method, and the read could not say which method the
    // total it quoted was for. A choice a kit hides is shown by its label, so either being visible counts.
    const CHOICES = 'input[type="radio"],input[type="checkbox"],[role="radio"],[role="checkbox"],[role="switch"]';
    for (const e of root.querySelectorAll(CHOICES)) {
      const native = e.tagName === 'INPUT';
      if (!(native ? e.checked : e.getAttribute('aria-checked') === 'true')) continue;
      if (!visible(e) && ![...(e.labels || [])].some(visible)) continue;
      const name = nameOf(e);
      if (name) push('observation', `${name}: checked`);
    }
    // Images carry no innerText, so a text-only capture cannot distinguish their presence from absence. What it can
    // say is kept as data in the one line: a sentence that pixels are not described was cut as a span and quoted as a
    // fact, so a Street View run answered "Pixels are not described" 33 times over a task about what the view shows.
    for (const image of [...root.querySelectorAll('img')].filter(visible)) {
      // Loading changes without a page edit, so captures report presence rather than transient decode state.
      const metadata = { alt: image.getAttribute('alt') ?? null,
        src: imageSource(image.getAttribute('src') ?? image.getAttribute('srcset')), pixels_described: false };
      push('observation', `Image element DOM metadata: ${JSON.stringify(metadata)}`);
    }
    // A player's clock and whether it plays are element state, drawn by native controls the text never holds. Each
    // read records them, so two reads of a playing player are two citable times. Hidden players count: they can play.
    const seconds = value => (Number.isFinite(value) ? Math.round(value * 10) / 10 : null);
    for (const player of root.querySelectorAll('audio,video')) {
      let heading = null;
      for (const h of root.querySelectorAll('h1,h2,h3,h4,h5,h6')) {
        if (!(h.compareDocumentPosition(player) & Node.DOCUMENT_POSITION_FOLLOWING)) break;
        heading = textOf(h) || heading;
      }
      // Its children are fallback text for browsers without players, not its name.
      const named = player.hasAttribute('aria-label') || player.hasAttribute('aria-labelledby');
      const state = {
        label: (named ? nameOf(player) : clean(player.title || '')) || null,
        heading,
        paused: player.paused,
        ended: player.ended,
        muted: player.muted,
        elapsed_seconds: seconds(player.currentTime) ?? 0,
        duration_seconds: seconds(player.duration),
      };
      const kind = player.tagName === 'AUDIO' ? 'Audio' : 'Video';
      push('observation', `${kind} element state: ${JSON.stringify(state)}`);
    }
    // A link drawn only as an icon has no text, so its destination, the one thing it says, was nowhere a reader
    // could quote: asked for a release's download URL beside an icon-only download link, a run read every other
    // field of the row and then clicked the link again and again looking for the address. The row's text names
    // which item the link belongs to, since the observation is not read where the link stands.
    for (const link of [...root.querySelectorAll('a[href]')].filter(visible)) {
      if (clean(link.innerText ?? '')) continue;
      // An icon button written as `href="#"` or a script URL runs code here and leads nowhere to report. A bare
      // `#` parses to an empty hash, so the raw attribute says it is a fragment link, not the parsed URL.
      let target;
      try {
        target = new URL(link.getAttribute('href'), location.href);
      } catch {
        // One malformed address must not cost the page its whole capture.
        continue;
      }
      const page = location.href.split('#')[0];
      const fragment = link.getAttribute('href').trim().startsWith('#') || (target.hash && target.href.split('#')[0] === page);
      if (target.protocol === 'javascript:' || fragment) continue;
      let row = link.parentElement;
      // An empty image paragraph must not borrow another paragraph's text from its enclosing article.
      while (row && row !== root && row.tagName !== 'P' && !clean(row.innerText ?? '')) row = row.parentElement;
      if (row === root) row = null;
      const metadata = {
        label: nameOf(link) || drawn(link) || null,
        href: imageSource(link.href),
        near: row ? textOf(row).replace(/\s+/g, ' ') || null : null,
      };
      // choices.rs (LINK_METADATA) keeps this out of a read's answer candidates by its opening words.
      push('observation', `Link element DOM metadata: ${JSON.stringify(metadata)}. The link has no text.`);
    }
    if (root === document.body) {
      const doc = document.documentElement;
      push('observation', `Observed viewport: Horizontal overflow: ${doc.scrollWidth > doc.clientWidth ? 'yes' : 'no'}`);
    }
    for (const e of root.querySelectorAll('*')) {
      if (e.shadowRoot) scope(e.shadowRoot, frame, `${source}/shadow:${identity(e)}`);
      // A frame the walk never reached (hidden, or inside a record or table read as one block) is still read.
      if (FRAMES.has(e.tagName) && !entered.has(e)) enter(e, []);
    }
    [path, floor, framePath, sourcePath] = previous;
  }

  scope(document.body, null, '');
  return { url: location.href, title: document.title, blocks, inaccessible_frames: inaccessible };
})
