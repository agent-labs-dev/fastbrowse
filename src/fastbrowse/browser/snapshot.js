// Ported from jev-ultrafast (MIT): jev_ultrafast/snapshot.js.
// Atomic single-evaluate observation of one frame's controls, viewport text, and a freshness fingerprint.
// Adapted to fastbrowse's Control shape (role + operation set, not per-kind actions) and to traverse
// open shadow roots. Runs once per frame session (main frame or an OOPIF); the Python side merges frames.
(mode => {
  const registry = window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  if (!registry.track) {
    const roots = new WeakSet();
    const changed = () => { registry.lastMutation = performance.now(); };
    const observer = new MutationObserver(changed);
    registry.track = root => {
      if (roots.has(root)) return;
      roots.add(root);
      observer.observe(root, { subtree: true, childList: true, characterData: true, attributes: true });
      // Property changes and scrolling can affect controls without producing mutation records.
      for (const event of ['input', 'change', 'scroll']) root.addEventListener(event, changed, true);
      changed();
    };
    registry.track(document);
  }
  if (mode === 'fingerprint') {
    let hash = 2166136261;
    const include = root => {
      registry.track(root);
      const text = root.body?.innerText ?? root.textContent ?? '';
      for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
      // innerText omits shadow trees and child documents even when their content is visible.
      for (const e of root.querySelectorAll('*')) {
        if (e.shadowRoot) include(e.shadowRoot);
        if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
          let inner = null;
          try { inner = e.contentDocument; } catch { inner = null; }
          if (inner?.body) include(inner);
        }
      }
    };
    include(document);
    return {
      fingerprint: location.href + '|' + document.title + '|' + (hash >>> 0) + '|' + scrollY,
      ready: document.readyState === 'interactive' || document.readyState === 'complete',
      quietFor: performance.now() - registry.lastMutation,
      hidden: document.hidden,
    };
  }
  if (!document.body) return null;
  const identity = e => {
    if (!registry.ids.has(e)) registry.ids.set(e, registry.next++);
    const id = registry.ids.get(e);
    registry.nodes.set(id, e);
    return id;
  };
  for (const [id, e] of registry.nodes) if (!e.isConnected) registry.nodes.delete(id);

  const safe = e => e.type !== 'hidden';
  // Password and agent-typed secret values never leave the page: only their length is observed.
  const secret = e => e.type === 'password' || e.dataset?.fastbrowseSecret === '1';
  const reveal = e => typeof e.value !== 'string' ? null : secret(e) ? '•'.repeat(e.value.length) : e.value;
  const visible = e => !e.closest('[aria-hidden="true"],[inert]') &&
    e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true });
  // Styled checkboxes and radios often hide the native input. Its visible label is the click
  // target, but the input still owns the checked/disabled state and must participate in freshness.
  const sourceOf = e => e.tagName === 'LABEL' && ['checkbox', 'radio'].includes(e.control?.type) ? e.control : e;

  const labelOf = (e, seen = new Set()) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    const root = typeof e.getRootNode === 'function' ? e.getRootNode() : document;
    const byId = id => (root.getElementById ? root.getElementById(id) : document.getElementById(id));
    const referenced = (e.getAttribute('aria-labelledby') || '').split(/\s+/)
      .map(id => labelOf(byId(id), seen)).filter(Boolean).join(' ');
    return referenced || e.getAttribute('aria-label') ||
      [...(e.labels || [])].map(l => labelOf(l, seen)).filter(Boolean).join(' ') ||
      (['button', 'submit', 'reset'].includes(e.type) ? e.value : '') || e.getAttribute('alt') ||
      (e.tagName === 'INPUT' ? '' : [...e.childNodes].map(n => n.nodeType === 3 ? n.textContent :
        n.nodeType === 1 && n.getAttribute('aria-hidden') !== 'true' ? labelOf(n, seen) : '').join(' ').trim()) ||
      e.getAttribute('title') || e.getAttribute('placeholder') || '';
  };

  const ARIA_ROLES = ['button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'menuitemradio',
    'option', 'gridcell', 'combobox', 'textbox', 'searchbox', 'spinbutton'];
  const SELECTOR = 'a[href],button,input,textarea,select,summary,label,[contenteditable="true"],' +
    ARIA_ROLES.map(role => `[role="${role}"]`).join(',');

  const roleOf = e => {
    if (sourceOf(e) !== e) return sourceOf(e).type;
    const explicit = e.getAttribute('role');
    if (ARIA_ROLES.includes(explicit)) return explicit;
    if (e.tagName === 'BUTTON' || e.tagName === 'SUMMARY') return 'button';
    if (e.tagName === 'A') return 'link';
    if (e.tagName === 'SELECT') return 'combobox';
    if (e.tagName === 'TEXTAREA' || e.isContentEditable) return 'textbox';
    if (e.tagName === 'INPUT') {
      if (e.type === 'file') return 'textbox';
      if (['checkbox', 'radio'].includes(e.type)) return e.type;
      if (['button', 'submit', 'reset', 'image'].includes(e.type)) return 'button';
      if (e.type === 'search') return 'searchbox';
      if (e.type === 'number') return 'spinbutton';
      if (['text', 'email', 'url', 'tel', 'password'].includes(e.type)) return 'textbox';
    }
    return null;
  };

  const submitSemantics = e => {
    const form = e.form;
    const implicit = ['text', 'search', 'url', 'tel', 'email', 'password', 'date', 'month', 'week',
      'time', 'datetime-local', 'number'];
    if (!form || e.tagName !== 'INPUT' || !implicit.includes(e.type)) return null;
    const submit = [...form.getRootNode().querySelectorAll('button,input')]
      .find(c => c.form === form && (c.type === 'submit' || c.type === 'image'));
    if (submit?.matches(':disabled')) return null;
    if (!submit && [...form.elements].filter(c => c.tagName === 'INPUT' && implicit.includes(c.type)).length > 1)
      return null;
    return JSON.stringify({ label: submit ? labelOf(submit) : labelOf(form),
      method: submit?.getAttribute('formmethod') || form.method,
      action: submit?.getAttribute('formaction') || form.action,
      text: form.innerText.slice(0, 2000) });
  };

  const framePath = doc => {
    if (doc === document) return null;
    const owner = doc.defaultView.frameElement;
    return `${framePath(owner.ownerDocument) || 'root'}/${identity(owner)}`;
  };
  registry.framePath = framePath;
  registry.submitSemantics = submitSemantics;

  // Traverse light DOM plus any open shadow roots, and same-origin same-process nested iframes.
  let inaccessible = 0;
  function* walk(root) {
    // A document observer cannot see mutations inside shadow roots or child documents.
    registry.track(root);
    for (const e of root.querySelectorAll(SELECTOR)) yield e;
    for (const e of root.querySelectorAll('*')) {
      if (e.shadowRoot) yield* walk(e.shadowRoot);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        let inner = null;
        try { inner = e.contentDocument; } catch { inner = null; }
        if (inner && inner.body) yield* walk(inner);
        else inaccessible++;
      }
    }
  }

  registry.guard = e => {
    if (!e?.isConnected || !visible(e)) return null;
    const source = sourceOf(e);
    if (source !== e && (source.matches(':disabled') || source.closest('[aria-disabled="true"],[inert]'))) return null;
    const scope = e.closest('form,dialog,[role="dialog"],article,li,tr,[role="row"]') || e.parentElement;
    return [identity(e), roleOf(e), labelOf(e), reveal(source), source.checked ?? null, e.selectedIndex ?? null,
      e.readOnly ?? null, e.matches(':disabled'), e.getAttribute('aria-disabled'),
      e.getAttribute('aria-expanded'), e.getAttribute('aria-checked'), e.getAttribute('aria-selected'),
      e.getAttribute('href'), scope?.innerText?.slice(0, 6000) || '',
      e.ownerDocument.location.origin, e.ownerDocument.defaultView.performance.timeOrigin, submitSemantics(e),
      identity(source)];
  };

  const controls = [];
  for (const e of walk(document)) {
    if (!safe(e) || !visible(e) || e.matches(':disabled') || e.closest('[aria-disabled="true"],[inert]')) continue;
    const source = sourceOf(e);
    if (source !== e && (visible(source) || source.matches(':disabled') ||
      source.closest('[aria-disabled="true"],[inert]'))) continue;
    const r = e.getBoundingClientRect(), x = r.x + r.width / 2, y = r.y + r.height / 2, rname = roleOf(e);
    if (!rname || r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth) continue;
    if (rname === 'gridcell' && e.querySelector('button,[role="button"]')) continue;
    const id = identity(e);
    const base = {
      id, role: rname, label: labelOf(e) || rname, offscreen: y < 0 || y >= innerHeight,
      distance: (y < 0 || y >= innerHeight) ? 1 + Math.abs(y - innerHeight / 2) : 0,
      sensitive: secret(source), input_type: source.type || null,
      frame_origin: e.ownerDocument.location.origin, frame_path: framePath(e.ownerDocument),
      submit_semantics: submitSemantics(e),
    };
    if (rname === 'link' && e.href) {
      const u = new URL(e.href, location.href);
      base.href = (u.origin === location.origin ? u.pathname + u.search : u.host + u.pathname).slice(0, 200);
    }
    for (const key of ['checked', 'selected', 'expanded']) {
      const value = e.getAttribute('aria-' + key);
      if (value !== null) base[key] = value === 'true';
    }
    if (['checkbox', 'radio'].includes(source.type)) base.checked = source.checked;
    // A field a form will not submit without: required and still empty, or marked invalid by the page.
    if (source.getAttribute('aria-invalid') === 'true' ||
      ((source.required || source.getAttribute('aria-required') === 'true') &&
        !(['checkbox', 'radio'].includes(source.type) ? source.checked : (source.value ?? '').trim()))) {
      base.blocking = true;
    }
    if (e.tagName === 'SELECT') {
      base.operations = ['select'];
      base.options = [...e.options].filter(o => !o.disabled && !o.closest('optgroup[disabled]')).map(o => o.label);
      base.value = [...e.selectedOptions].map(o => o.label).join(', ');
    } else if (e.type === 'file') {
      base.operations = ['upload'];
      base.value = null;
    } else {
      const editable = !e.readOnly && e.getAttribute('aria-readonly') !== 'true' &&
        (['textbox', 'searchbox', 'spinbutton'].includes(rname) ||
          (rname === 'combobox' && ['INPUT', 'TEXTAREA'].includes(e.tagName)));
      // reveal() is null for <li>, <progress> and <meter>, whose numeric `value`s are not field contents.
      base.value = reveal(source) ?? (e.isContentEditable || rname === 'combobox' ? e.innerText.trim() : null);
      base.operations = editable ? ['fill', 'click', 'enter'] : ['click'];
    }
    controls.push(base);
  }

  // Controls that read the same cannot be told apart by their label, and a hint naming the right one
  // ("Add to cart under Sauce Labs Backpack") has nothing to match on. Give each twin the widest ancestor
  // that holds it and none of its twins, named by that ancestor's heading or its own first line of text.
  // Only labels that actually collide pay for it.
  const HEADINGS = 'h1,h2,h3,h4,h5,h6,[role="heading"],legend,caption,th,dt,summary';
  const firstLine = text => (text || '').split('\n').map(s => s.replace(/\s+/g, ' ').trim()).find(Boolean) || '';
  const nameOf = (scope, label) => {
    const aria = (scope.getAttribute('aria-label') || '').trim();
    if (aria && aria !== label) return aria;
    for (const heading of scope.querySelectorAll(HEADINGS)) {
      const text = firstLine(heading.innerText);
      if (text && text !== label) return text;
    }
    const text = scope.innerText || '';
    return firstLine(label ? text.split(label).join(' ') : text);
  };
  const contextOf = (element, twins, label) => {
    // The widest twin-free ancestor is the card, row or section the twins repeat over. A narrower one names
    // the button's own wrapper, which on a shop is its price rather than the product it belongs to.
    let scope = null;
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      if (twins.some(twin => twin !== element && e.contains(twin))) break;
      scope = e;
    }
    return scope ? nameOf(scope, label).slice(0, 120) : '';
  };
  // Content a stylesheet shows only under the pointer (`.card:hover .caption`) is out of reach of every other
  // operation. An element whose hover rule would reveal something now hidden is offered as a hover target.
  const docs = [];
  const addDoc = doc => {
    if (!doc?.body || docs.includes(doc)) return;
    docs.push(doc);
    for (const frame of doc.querySelectorAll('iframe,frame')) {
      try { addDoc(frame.contentDocument); } catch { /* A cross-origin frame is observed in its own session. */ }
    }
  };
  addDoc(document);
  // Parsing every stylesheet each step is the slow part on a large site, so a document's hover rules are kept
  // until its rule counts change (a new sheet, or rules a script inserted).
  registry.hoverRules ||= new WeakMap();
  const revealsIn = doc => {
    const sheets = [];
    for (const sheet of doc.styleSheets) {
      try { sheets.push(sheet.cssRules); } catch { /* A cross-origin stylesheet cannot be read. */ }
    }
    const version = sheets.map(rules => rules.length).join(',');
    const cached = registry.hoverRules.get(doc);
    if (cached?.version === version) return cached.reveals;
    const reveals = new Map();
    const collect = rules => {
      for (const rule of rules) {
        if (rule.cssRules?.length) collect(rule.cssRules);
        for (const selector of (rule.selectorText || '').split(',')) {
          // Split `a:hover b` into the hovered compound and what it reveals inside it. Rules that restyle the
          // hovered element itself, or reveal a sibling or a pseudo-element, have nothing to offer here.
          const m = selector.match(/^(.*?):hover([^\s>+~]*)\s*(>?)\s*([^+~]*)$/);
          if (!m || !m[1].trim() || !m[4].trim() || m[4].includes('::')) continue;
          const pair = [(m[1] + m[2]).replaceAll(':hover', '').trim(), `:scope ${m[3]} ${m[4].replaceAll(':hover', '')}`];
          reveals.set(pair.join('\n'), pair);
        }
      }
    };
    for (const rules of sheets) collect(rules);
    const found = [...reveals.values()].slice(0, 200);
    registry.hoverRules.set(doc, { version, reveals: found });
    return found;
  };
  const offered = new Set(controls.map(c => registry.nodes.get(c.id)));
  // Bounded work: each candidate costs a query, and a stylesheet can name thousands of hoverable elements.
  let hovers = 0, checked = 0;
  for (const doc of docs) {
    for (const [base, tail] of revealsIn(doc)) {
      let hosts = [];
      try { hosts = doc.querySelectorAll(base); } catch { continue; }
      for (const e of hosts) {
        if (hovers >= 40 || checked >= 400) break;
        // A link or button's own hover rule restyles it or shows decoration; offering HOVER there as well as
        // CLICK only gives the choice another way to be wrong.
        if (offered.has(e) || e === doc.body || e === doc.documentElement || !visible(e)) continue;
        const r = e.getBoundingClientRect(), x = r.x + r.width / 2, y = r.y + r.height / 2;
        if (r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth || r.height > innerHeight) continue;
        checked++;
        let hidden = false;
        try {
          hidden = [...e.querySelectorAll(tail)].some(t => !visible(t) && (t.textContent.trim() || t.querySelector('img,a')));
        } catch { continue; }
        if (!hidden) continue;
        const image = e.querySelector('img[alt]');
        const label = firstLine(e.innerText) || e.getAttribute('aria-label') || image?.getAttribute('alt') ||
          e.getAttribute('title') || e.tagName.toLowerCase();
        const role = e.getAttribute('role') ||
          { FIGURE: 'figure', LI: 'listitem', IMG: 'img', TR: 'row', TD: 'cell' }[e.tagName] || 'group';
        const c = {
          id: identity(e), role, label: label.slice(0, 200), offscreen: y < 0 || y >= innerHeight,
          distance: (y < 0 || y >= innerHeight) ? 1 + Math.abs(y - innerHeight / 2) : 0,
          sensitive: false, input_type: null, frame_origin: e.ownerDocument.location.origin,
          frame_path: framePath(e.ownerDocument), submit_semantics: null, value: null, operations: ['hover'],
        };
        controls.push(c);
        offered.add(e);
        hovers++;
      }
    }
  }

  const byLabel = new Map();
  for (const c of controls) {
    const key = JSON.stringify([c.role, c.label]);
    if (!byLabel.has(key)) byLabel.set(key, []);
    byLabel.get(key).push(c);
  }
  for (const group of byLabel.values()) {
    if (group.length < 2) continue;
    const twins = group.map(c => registry.nodes.get(c.id));
    for (const c of group) {
      const context = contextOf(registry.nodes.get(c.id), twins, c.label);
      if (context) c.context = context;
    }
    // Twins whose surroundings name none of them, like a row of identical avatars, still differ by position.
    if (group.every(c => !c.context)) {
      twins.sort((a, b) => a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1);
      for (const c of group) {
        c.context = `${twins.indexOf(registry.nodes.get(c.id)) + 1} of ${twins.length}`;
      }
    }
  }

  // Python applies the configured caps after merging frames; keep the nearest controls first.
  controls.sort((a, b) => a.distance - b.distance);

  // Text in a same-process child document is on screen as much as the parent's: a frameset page has no text of
  // its own at all. Each child is read in its own coordinates, shifted by where its frame sits.
  const words = [];
  const readText = (doc, dx, dy) => {
    if (!doc.body) return;
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
    const range = doc.createRange();
    let node;
    while ((node = walker.nextNode())) {
      const value = node.textContent.trim(), parent = node.parentElement;
      if (!value || !parent || parent.closest('script,style,noscript,template') || !visible(parent)) continue;
      range.selectNodeContents(node);
      const r = range.getBoundingClientRect();
      const top = r.top + dy, left = r.left + dx;
      if (r.width > 0 && r.height > 0 && top + r.height > 0 && top < innerHeight && left + r.width > 0
        && left < innerWidth) {
        words.push(value);
      }
    }
    for (const frame of doc.querySelectorAll('iframe,frame')) {
      let inner = null;
      try { inner = frame.contentDocument; } catch { inner = null; }
      if (!inner) continue;
      const f = frame.getBoundingClientRect();
      readText(inner, dx + f.left + frame.clientLeft, dy + f.top + frame.clientTop);
    }
  };
  readText(document, 0, 0);
  const viewport_text = words.join('\n');

  const guards = {};
  for (const c of controls) guards[c.id] = registry.guard(registry.nodes.get(c.id));
  const field_state = [...document.querySelectorAll('input,textarea,select')].filter(safe)
    .map(e => [identity(e), reveal(e), e.checked, e.selectedIndex, e.disabled, e.readOnly]);
  // Compare meaning and identity for the page_key fingerprint; geometry is re-resolved just before input.
  const semantics = controls.map(({ distance, ...c }) => c);
  const page_key = JSON.stringify([location.href, scrollX, scrollY, innerWidth, innerHeight,
    document.title, viewport_text, semantics, field_state]);

  return {
    url: location.href, title: document.title, viewport_text, document_key: String(performance.timeOrigin),
    controls: semantics, page_key, guards, inaccessible_frames: inaccessible,
  };
})
