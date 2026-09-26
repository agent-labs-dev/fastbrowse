// Ported from jev-ultrafast (MIT): jev_ultrafast/snapshot.js.
// Atomic single-evaluate observation of one frame's controls, viewport text, and a freshness fingerprint.
// Adapted to fastbrowse's Control shape (role + operation set, not per-kind actions) and to traverse
// open shadow roots. Runs once per frame session (main frame or an OOPIF); the Python side merges frames.
(mode => {
  // Form context is repeated for each submit-capable input in safety prompts.
  const FORM_TEXT_CHARS = 2000;
  // Freshness guards bound comparison work on large cards; this text is never sent to a model.
  const GUARD_TEXT_CHARS = 6000;
  // Twin controls repeat their card heading, so keep its excerpt shorter than the control itself.
  const CONTROL_CONTEXT_CHARS = 120;
  // A hover target can be a whole card whose text would crowd out the other controls.
  const HOVER_LABEL_CHARS = 200;
  // Each hover rule fans out to DOM queries, so bound stylesheet work per observation.
  const MAX_HOVER_RULES = 200;
  // Hover targets supplement the actionable controls rather than fill their observation budget.
  const MAX_HOVER_TARGETS = 40;
  // Each candidate checks hidden descendants, which can stall a dense page's observation.
  const MAX_HOVER_CHECKS = 400;
  const excerpt = (text, cap) => text.length <= cap ? text :
    `${text.slice(0, cap)} [${text.length - cap} characters omitted]`;
  // A transparent native checkbox is the one control TodoMVC shows, so only its own opacity is excused: a
  // transparent ancestor still hides it. Walked by style, not layout box, as a `display: contents` parent
  // generates no box yet hides nothing.
  const nativeChoice = e => e.tagName === 'INPUT' && ['checkbox', 'radio'].includes(e.type);
  const shownThrough = e => {
    for (let a = e.parentElement ?? e.getRootNode().host; a; a = a.parentElement ?? a.getRootNode().host)
      if (a.ownerDocument.defaultView.getComputedStyle(a).opacity === '0') return false;
    return true;
  };
  const registry = window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  // What was offered to be acted on; `ids` also names hover targets and fingerprinted nodes.
  registry.controls ||= new WeakSet();
  // A visible loading indicator means the page is still fetching what it will draw, which network idle and a
  // quiet DOM both report as settled: a spinner mutates nothing while it spins. Named classes are a heuristic
  // and deliberately so -- the wait on them is bounded and expires into proceeding, so a false match costs
  // time and never correctness. Hidden indicators are ignored because most pages keep one in the DOM always.
  const LOADING = '[aria-busy="true"],[role="progressbar"],progress:not([value]),' +
    ['spinner', 'loading', 'loader', 'skeleton', 'shimmer']
      .flatMap(name => [`[class*="${name}" i]`, `[id*="${name}" i]`]).join(',');
  // Only an indicator on screen is waited on. GitHub keeps an empty fixed progress bar and lazy skeletons below
  // the fold, which load only once scrolled to: waiting on them spent the full wait on every GitHub page.
  const showing = e => {
    if (!e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const r = e.getBoundingClientRect();
    const view = e.ownerDocument.defaultView || window;
    return r.width > 0 && r.height > 0 && r.bottom > 0 && r.right > 0 && r.top < view.innerHeight &&
      r.left < view.innerWidth;
  };
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
    let loading = false;
    const hashText = text => {
      for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
    };
    const include = root => {
      registry.track(root);
      loading ||= [...root.querySelectorAll(LOADING)].some(showing);
      const text = root.body?.innerText ?? root.textContent ?? '';
      hashText(text);
      // innerText omits shadow trees and child documents even when their content is visible.
      for (const e of root.querySelectorAll('*')) {
        // A filter can change only its checkmark, so text alone reported "Nonstop only" as a no-op.
        // Field values stay out: the run judges fills by the value it wrote, even when a popup changes.
        // Only rendered controls: a hidden carousel's aria-selected dots churn on their own and would read as
        // progress.
        if (('checked' in e || 'selected' in e || e.hasAttribute('aria-checked') || e.hasAttribute('aria-selected')) &&
          e.checkVisibility({ checkOpacity: !nativeChoice(e), checkVisibilityCSS: true }) &&
          (!nativeChoice(e) || shownThrough(e)))
          hashText(JSON.stringify([e.checked ?? null, e.selected ?? null,
            e.getAttribute('aria-checked'), e.getAttribute('aria-selected')]));
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
      loading,
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
  const visible = e => {
    // Todo lists paint the checkmark beside a transparent native input without associating a label.
    // The input still receives clicks; opacity alone must not remove the only control for that row.
    // A transparent input with a visible label of its own keeps the label as its click target: TodoMVC's
    // "Mark all as complete" input is 1px and off screen, and only its label can be clicked.
    const choice = nativeChoice(e) &&
      e.ownerDocument.defaultView.getComputedStyle(e).pointerEvents !== 'none' &&
      ![...(e.labels || [])].some(l => l.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }));
    if (e.closest('[aria-hidden="true"],[inert]') ||
      !e.checkVisibility({ checkOpacity: !choice, checkVisibilityCSS: true }) || (choice && !shownThrough(e))) return false;
    if (!choice) return true;
    const r = e.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && !e.matches(':disabled') && !e.closest('[aria-disabled="true"]');
  };
  registry.visible = visible;
  // Styled checkboxes and radios often hide the native input. Its visible label is the click
  // target, but the input still owns the checked/disabled state and must participate in freshness.
  const sourceOf = e => e.tagName === 'LABEL' && ['checkbox', 'radio'].includes(e.control?.type) ? e.control : e;

  // Their text is code, not a name: Amazon nests a <style> inside a result card's link.
  const CODE = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE']);
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
        n.nodeType === 1 && !CODE.has(n.tagName) && n.getAttribute('aria-hidden') !== 'true' ? labelOf(n, seen) : '')
        .join(' ').trim()) ||
      e.getAttribute('title') || e.getAttribute('placeholder') || '';
  };

  const ARIA_ROLES = ['button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'menuitemradio',
    'option', 'gridcell', 'combobox', 'textbox', 'searchbox', 'spinbutton'];
  // jQuery UI's datepicker prev/next are anchors with no href, appended to <body> once the field is focused:
  // they carry no destination, only a click handler, so they need naming here to be walked at all.
  const SELECTOR = 'a[href],a[data-handler][data-event="click"],a[onclick],a[role="button"],' +
    'button,input,textarea,select,summary,label,[contenteditable="true"],' +
    ARIA_ROLES.map(role => `[role="${role}"]`).join(',');
  // Types whose value is a calendar date, not free text: the field writer must produce the type's ISO shape
  // and the native value setter, not typed keystrokes, is what a picker widget actually commits.
  const DATE_TYPES = ['date', 'datetime-local', 'month', 'week', 'time'];

  const roleOf = e => {
    if (sourceOf(e) !== e) return sourceOf(e).type;
    const explicit = e.getAttribute('role');
    if (ARIA_ROLES.includes(explicit)) return explicit;
    if (e.tagName === 'BUTTON' || e.tagName === 'SUMMARY') return 'button';
    // An anchor with nowhere to go acts through its click handler alone, like the jQuery UI datepicker's
    // Next/Prev: a destination is what makes it a link, so without one it is offered as a button instead.
    if (e.tagName === 'A') return e.hasAttribute('href') ? 'link' : 'button';
    if (e.tagName === 'SELECT') return 'combobox';
    if (e.tagName === 'TEXTAREA' || e.isContentEditable) return 'textbox';
    if (e.tagName === 'INPUT') {
      if (e.type === 'file') return 'textbox';
      if (['checkbox', 'radio'].includes(e.type)) return e.type;
      if (['button', 'submit', 'reset', 'image'].includes(e.type)) return 'button';
      if (e.type === 'search') return 'searchbox';
      if (e.type === 'number') return 'spinbutton';
      if (['text', 'email', 'url', 'tel', 'password', ...DATE_TYPES].includes(e.type)) return 'textbox';
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
      text: excerpt(form.innerText, FORM_TEXT_CHARS) });
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
    // An absent aria-disabled and "false" both mean enabled. Google Flights adds the "false" as it hydrates, and
    // the raw attribute turned the run's first click stale.
    return [identity(e), roleOf(e), labelOf(e), reveal(source), source.checked ?? null, e.selectedIndex ?? null,
      e.readOnly ?? null, e.matches(':disabled'), e.getAttribute('aria-disabled') === 'true',
      e.getAttribute('aria-expanded'), e.getAttribute('aria-checked'), e.getAttribute('aria-selected'),
      e.getAttribute('href'), scope?.innerText?.slice(0, GUARD_TEXT_CHARS) || '',
      e.ownerDocument.location.origin, e.ownerDocument.defaultView.performance.timeOrigin, submitSemantics(e),
      identity(source)];
  };

  const controls = [];
  const fieldScope = e => {
    if (!e.matches('input,textarea,select')) return null;
    const form = e.form || e.closest('form,fieldset,[role="form"]');
    if (form) return String(identity(form));
    // Script-built forms can use a local group of labelled fields without an HTML form owner.
    // Stop at document sections so a sidebar search never joins the content's fields.
    for (let p = e.parentElement; p && !p.matches('body,main,article,section,aside,nav,header,footer');
      p = p.parentElement) {
      if (p.querySelectorAll('input:not([type=hidden]),textarea,select').length > 1) return String(identity(p));
    }
    return null;
  };
  for (const e of walk(document)) {
    if (!safe(e) || !visible(e) || e.matches(':disabled') || e.closest('[aria-disabled="true"],[inert]')) continue;
    const source = sourceOf(e);
    if (source !== e && (visible(source) || source.matches(':disabled') ||
      source.closest('[aria-disabled="true"],[inert]'))) continue;
    const r = e.getBoundingClientRect(), x = r.x + r.width / 2, y = r.y + r.height / 2, rname = roleOf(e);
    if (!rname || r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth) continue;
    if (rname === 'gridcell' && e.querySelector('button,[role="button"]')) continue;
    const id = identity(e);
    registry.controls.add(e);
    const base = {
      id, role: rname, label: labelOf(e) || rname, offscreen: y < 0 || y >= innerHeight,
      distance: (y < 0 || y >= innerHeight) ? 1 + Math.abs(y - innerHeight / 2) : 0,
      sensitive: secret(source), input_type: source.type || null,
      frame_origin: e.ownerDocument.location.origin, frame_path: framePath(e.ownerDocument),
      form_id: fieldScope(source),
      submit_semantics: submitSemantics(e),
    };
    if (rname === 'link' && e.href) {
      const u = new URL(e.href, location.href);
      base.href = u.origin === location.origin ? u.pathname + u.search : u.host + u.pathname;
      // The page's own word for "the next page of this list", which survives an icon label or another language.
      if (e.rel && e.rel.split(/\s+/).includes('next')) base.next_page = true;
    }
    for (const key of ['checked', 'selected', 'expanded']) {
      const value = e.getAttribute('aria-' + key);
      if (value !== null) base[key] = value === 'true';
    }
    if (['checkbox', 'radio'].includes(source.type)) base.checked = source.checked;
    // A field a form will not submit without: required and still empty, or marked invalid by the page.
    // A custom widget marks the element the user sees, not the input underneath it, so both are asked.
    if (e.getAttribute('aria-invalid') === 'true' || source.getAttribute('aria-invalid') === 'true' ||
      (['INPUT', 'SELECT', 'TEXTAREA'].includes(source.tagName) &&
        (source.required || source.getAttribute('aria-required') === 'true') &&
        !(base.checked ?? (source.value ?? '').trim()))) {
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
  // A label tied to no control titles what follows it, as a heading would: a blog post's "Date Picker 3" is one,
  // and the post's first heading ("Data Entry Form") named that picker's Submit when only headings counted.
  // A hidden title names nothing on screen: innerText still returns a `hidden` label's text.
  const titles = scope => [...scope.querySelectorAll(`${HEADINGS},label`)].filter(
    e => (e.localName !== 'label' || !e.control) && e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })
  );
  // A table's header cell names its own row, and a caption its own table, never what sits outside them: a jQuery UI
  // datepicker's Next button was named "Su", its weekday header, and every month's Next read the same.
  const reaches = (title, element) =>
    !['th', 'caption'].includes(title.localName) || title.parentElement.contains(element);
  // The title nearest before the element names its section; the scope's first title is the fallback, and the
  // only answer when `nearest` is false.
  const sectionOf = (scope, element, label, nearest = true) => {
    let first = '', before = '';
    for (const title of titles(scope)) {
      const text = firstLine(title.innerText);
      if (!text || text === label || title.contains(element) || !reaches(title, element)) continue;
      first ||= text;
      if (title.compareDocumentPosition(element) & Node.DOCUMENT_POSITION_FOLLOWING) before = text;
    }
    return (nearest && before) || first;
  };
  // Labels more than one control shares, filled in once every control is listed.
  const repeated = new Set();
  const nameOf = (scope, element, label, nearest) => {
    const aria = (scope.getAttribute('aria-label') || '').trim();
    if (aria && aria !== label) return aria;
    const section = sectionOf(scope, element, label, nearest);
    if (section) return section;
    // A line that is itself a repeated control's label ("Prev") reads the same in every repeat, so it names none:
    // past it, a datepicker's first line is the month it shows. The label is cut out as whole words, since a day
    // labelled "2" cut out of "October 2026" left "October 0 6".
    const lines = (scope.innerText || '').split('\n').map(line => line.replace(/\s+/g, ' ').trim())
      .filter(line => line && !repeated.has(line));
    return firstLine(lines.map(line => label ? ` ${line} `.split(` ${label} `).join(' ') : line).join('\n'));
  };
  const contextOf = (element, twins, label, nearest = true) => {
    // The widest twin-free ancestor is the card, row or section the twins repeat over. A narrower one names
    // the button's own wrapper, which on a shop is its price rather than the product it belongs to.
    let scope = null;
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      if (twins.some(twin => twin !== element && e.contains(twin))) break;
      scope = e;
    }
    return scope ? excerpt(nameOf(scope, element, label, nearest), CONTROL_CONTEXT_CHARS) : '';
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
    const found = [...reveals.values()].slice(0, MAX_HOVER_RULES);
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
        if (hovers >= MAX_HOVER_TARGETS || checked >= MAX_HOVER_CHECKS) break;
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
          id: identity(e), role, label: excerpt(label, HOVER_LABEL_CHARS), offscreen: y < 0 || y >= innerHeight,
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
  for (const group of byLabel.values()) if (group.length > 1) repeated.add(group[0].label);
  for (const group of byLabel.values()) {
    if (group.length < 2) continue;
    const twins = group.map(c => registry.nodes.get(c.id));
    const nearest = group.map(c => contextOf(registry.nodes.get(c.id), twins, c.label));
    group.forEach((c, i) => {
      // Cards that each end in an "Options" heading share their nearest title, which would name every twin
      // alike; a twin whose nearest title another twin shares is named by its card's first title instead.
      const shared = nearest.indexOf(nearest[i]) !== nearest.lastIndexOf(nearest[i]);
      const context = shared ? contextOf(registry.nodes.get(c.id), twins, c.label, false) : nearest[i];
      if (context) c.context = context;
    });
    // Twins whose surroundings name none of them, like a row of identical avatars, still differ by position.
    if (group.every(c => !c.context)) {
      twins.sort((a, b) => a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1);
      for (const c of group) {
        c.context = `${twins.indexOf(registry.nodes.get(c.id)) + 1} of ${twins.length}`;
      }
    }
  }

  // A date field's own label ("Start Date") rarely says which picker it belongs to when a page offers several
  // ("Date Picker 3"): unlike a colliding label, a uniquely named one never reaches the loop above, so the
  // nearest ancestor that names a section is attached here on its own, without the twin-avoidance `contextOf`
  // needs to keep a card from naming its neighbour.
  const nearestHeading = (element, label) => {
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      const aria = (e.getAttribute('aria-label') || '').trim();
      if (aria && aria !== label) return excerpt(aria, CONTROL_CONTEXT_CHARS);
      const section = sectionOf(e, element, label);
      if (section) return excerpt(section, CONTROL_CONTEXT_CHARS);
    }
    return '';
  };
  for (const c of controls) {
    if (!c.context && DATE_TYPES.includes(c.input_type)) {
      const context = nearestHeading(registry.nodes.get(c.id), c.label);
      if (context) c.context = context;
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
  // A dependent field can appear without changing page text. Compare all controls,
  // excusing only the value and required marker of the field just filled.
  const formState = controls.map(({ distance, offscreen, ...c }) => {
    const form = registry.nodes.get(c.id)?.form;
    return {...c, form: form ? [form.action, form.method, form.target] : null};
  });
  const formPage = JSON.stringify([location.href, performance.timeOrigin, document.title, viewport_text]);
  if (typeof mode === 'object') {
    const previous = registry.formObserved;
    if (!previous || previous.page !== formPage) return false;
    const expected = previous.controls.map(c => {
      if (c.id !== mode.id) return c;
      const {blocking, ...filled} = c;
      return mode.text ? {...filled, value: mode.text} : c;
    });
    const same = JSON.stringify(expected) === JSON.stringify(formState);
    if (same) registry.formObserved = {page: formPage, controls: formState};
    return same;
  }
  registry.formObserved = {page: formPage, controls: formState};
  // Kept so a later check can ask whether any of these controls changed without sending the guards back.
  registry.observed = new Map(controls.map(c => [c.id, JSON.stringify(guards[c.id])]));
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
