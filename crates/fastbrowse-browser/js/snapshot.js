// Ported from jev-ultrafast (MIT): jev_ultrafast/snapshot.js.
// Atomic single-evaluate observation of one frame's controls, viewport text, and a freshness fingerprint.
// Adapted to fastbrowse's Control shape (role + operation set, not per-kind actions) and to traverse
// open shadow roots. Runs once per frame session (main frame or an OOPIF); CdpPage merges frames.
// biome-ignore format: session.rs appends mode arguments to this parenthesized expression.
((mode, allowedOrigins = null) => {
  // Reading a cross-origin frame's document can throw.
  const childDocument = frame => {
    try {
      return frame.contentDocument;
    } catch {
      return null;
    }
  };
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
  // Freshness guards bound comparison work on large cards; this text is never sent to a model.
  const GUARD_TEXT_CHARS = 6000;
  // A transparent native checkbox is the one control TodoMVC shows, so only its own opacity is excused: a
  // transparent ancestor still hides it. Walked by style, not layout box, as a `display: contents` parent
  // generates no box yet hides nothing.
  const nativeChoice = e => e.tagName === 'INPUT' && ['checkbox', 'radio'].includes(e.type);
  const shownThrough = e => {
    for (let a = e.parentElement ?? e.getRootNode().host; a; a = a.parentElement ?? a.getRootNode().host)
      if (a.ownerDocument.defaultView.getComputedStyle(a).opacity === '0') return false;
    return true;
  };
  const registry = (window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 });
  // Keep the page's own timer overrides from holding a browser-side frame wait open.
  registry.nativeSetTimeout ||= window.setTimeout.bind(window);
  registry.nativeClearTimeout ||= window.clearTimeout.bind(window);
  registry.nativeRequestAnimationFrame ||= window.requestAnimationFrame.bind(window);
  // What was offered to be acted on; `ids` also names hover targets and fingerprinted nodes.
  registry.controls ||= new WeakSet();
  if (!registry.track) {
    const roots = new WeakSet();
    const changed = () => {
      registry.lastMutation = performance.now();
    };
    const observer = new MutationObserver(changed);
    registry.track = root => {
      if (roots.has(root)) return;
      roots.add(root);
      observer.observe(root, { subtree: true, childList: true, characterData: true, attributes: true });
      // Input handlers can defer a navigation or redraw without mutating yet. Start the quiet clock at the
      // event, so the reply's travel time counts toward settling even when the action changes nothing.
      for (const event of [
        'input',
        'change',
        'scroll',
        'wheel',
        'pointermove',
        'pointerdown',
        'pointerup',
        'keydown',
        'keyup',
      ])
        root.addEventListener(event, changed, true);
      changed();
    };
    registry.track(document);
  }
  const clock = () => ({
    ready: document.readyState === 'interactive' || document.readyState === 'complete',
    quietFor: performance.now() - registry.lastMutation,
    hidden: document.hidden,
  });
  // The settle loop reads the clock alone until the page has gone quiet: the walk below reads every element and
  // all the page's text, which on a large page costs more than the poll interval leaves room for.
  if (mode === 'quiet') return clock();
  if (mode === 'fingerprint') {
    let hash = 2166136261;
    const hashText = text => {
      for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
    };
    const include = root => {
      registry.track(root);
      hashText(root.body?.innerText ?? root.textContent ?? '');
      // innerText omits shadow trees and child documents even when their content is visible.
      for (const e of root.querySelectorAll('*')) {
        // A filter can change only its checkmark, so text alone reported "Nonstop only" as a no-op.
        // Field values stay out: the run judges fills by the value it wrote, even when a popup changes.
        // Only rendered controls: a hidden carousel's aria-selected dots churn on their own and would read as
        // progress.
        if (
          ('checked' in e || 'selected' in e || e.hasAttribute('aria-checked') || e.hasAttribute('aria-selected')) &&
          e.checkVisibility({ checkOpacity: !nativeChoice(e), checkVisibilityCSS: true }) &&
          (!nativeChoice(e) || shownThrough(e))
        )
          hashText(
            JSON.stringify([
              e.checked ?? null,
              e.selected ?? null,
              e.getAttribute('aria-checked'),
              e.getAttribute('aria-selected'),
            ]),
          );
        if (e.shadowRoot) include(e.shadowRoot);
        if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
          const inner = childDocument(e);
          if (inner?.body && granted(inner)) include(inner);
        }
      }
    };
    include(document);
    return {
      ...clock(),
      fingerprint: location.href + '|' + document.title + '|' + (hash >>> 0) + '|' + scrollY,
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
  // The DOM cannot change while this script runs, so a label, a text or a section title read once is reused for
  // the rest of it. `registry.guard` reads them again in a later script, against a page that may have changed
  // since, so the reuse ends as soon as this one does, whether it returns or throws.
  const caches = [];
  registry.nativeQueueMicrotask ||= window.queueMicrotask.bind(window);
  registry.nativeQueueMicrotask(() => {
    for (const cache of caches) cache.clear();
    caches.length = 0;
  });
  const memo = read => {
    const cache = new Map();
    caches.push(cache);
    return e => {
      if (!caches.includes(cache)) return read(e);
      if (!cache.has(e)) cache.set(e, read(e));
      return cache.get(e);
    };
  };
  const innerTextOf = memo(e => e.innerText);

  const safe = e => e.type !== 'hidden';
  // Password and agent-typed secret values never leave the page: only their length is observed.
  const secret = e => e.type === 'password' || e.dataset?.fastbrowseSecret === '1';
  const reveal = e => (typeof e.value !== 'string' ? null : secret(e) ? '•'.repeat(e.value.length) : e.value);
  // Asked of each control by the walk and again by its guard, and of each text node's parent.
  const visible = memo(e => {
    // Todo lists paint the checkmark beside a transparent native input without associating a label.
    // The input still receives clicks; opacity alone must not remove the only control for that row.
    // A transparent input with a visible label of its own keeps the label as its click target: TodoMVC's
    // "Mark all as complete" input is 1px and off screen, and only its label can be clicked.
    const choice =
      nativeChoice(e) &&
      e.ownerDocument.defaultView.getComputedStyle(e).pointerEvents !== 'none' &&
      ![...(e.labels || [])].some(l => l.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }));
    if (
      e.closest('[aria-hidden="true"],[inert]') ||
      !e.checkVisibility({ checkOpacity: !choice, checkVisibilityCSS: true }) ||
      (choice && !shownThrough(e))
    )
      return false;
    if (!choice) return true;
    const r = e.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && !e.matches(':disabled') && !e.closest('[aria-disabled="true"]');
  });
  registry.visible = visible;
  // Styled checkboxes and radios often hide the native input. Its visible label is the click
  // target, but the input still owns the checked/disabled state and must participate in freshness.
  const sourceOf = e => (e.tagName === 'LABEL' && ['checkbox', 'radio', 'file'].includes(e.control?.type) ? e.control : e);

  // Their text is code, not a name: Amazon nests a <style> inside a result card's link.
  const CODE = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE']);
  // Icon-only buttons hide their glyph from accessibility text, but the icon library still names it: in a class,
  // a sprite reference, a data attribute, an svg title or a ligature. A gallery's trash control otherwise reads
  // "button" beside an edit link that reads "link", and the choice cannot tell removing an image from editing it.
  const ICON_CLASS =
    /^(?:icon-tabler|tabler-icon|lucide|glyphicon|fa|fas|far|bi|mdi|icon|ti|la|ri|ion|feather)-([a-z][a-z0-9-]{0,47})$/;
  // Sizes, weights and effects share an icon library's prefix without naming a glyph, as does the library itself.
  const ICON_STYLE =
    /^(?:icon|tabler|solid|regular|light|thin|duotone|brands|xs|sm|lg|xl|\d+x|fw|spin|pulse|border|inverse|stack.*|pull-.*|rotate-.*|flip-.*)$/;
  const glyphName = text => {
    const name = (text || '').trim().toLowerCase().replace(/^(?:icons?|ico|ic|svg|sprite)[-_]/, '');
    return /^[a-z][a-z0-9_-]{0,47}$/.test(name) && !ICON_STYLE.test(name) ? name : '';
  };
  const iconLabel = e => {
    const glyphs = new Set();
    for (const icon of e.querySelectorAll('svg,i,span,[role="img"]')) {
      if (!icon.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) continue;
      const named = [];
      for (const token of icon.classList) {
        const glyph = ICON_CLASS.exec(token);
        if (glyph && !ICON_STYLE.test(glyph[1])) named.push(glyph[1]);
      }
      const use = icon.localName === 'svg' && icon.querySelector('use');
      const sprite = use && (use.getAttribute('href') || use.getAttribute('xlink:href') || '');
      if (sprite) named.push(glyphName(sprite.split('#').pop().split('/').pop().replace(/\.svg$/, '')));
      named.push(glyphName(icon.getAttribute('data-icon')));
      // A ligature font draws the glyph its text names, and the icon is hidden so readers skip that word.
      if ([...icon.classList].some(token => /^material-(?:icons|symbols)/.test(token))) {
        named.push(glyphName(icon.textContent.replace(/\s+/g, '_')));
      }
      const title =
        icon.getAttribute('aria-label') || (icon.localName === 'svg' ? icon.querySelector(':scope > title')?.textContent : '');
      if (title?.trim()) glyphs.add(title.replace(/\s+/g, ' ').trim());
      else for (const name of named) if (name) glyphs.add(`${name.replace(/[-_]/g, ' ')} icon`);
    }
    return glyphs.size === 1 ? [...glyphs][0] : '';
  };
  // An accessible description is what an unnamed control has left that says what it does.
  const describedBy = e => {
    const root = typeof e.getRootNode === 'function' ? e.getRootNode() : document;
    return (e.getAttribute('aria-describedby') || '')
      .split(/\s+/)
      .map(id => (id && (root.getElementById ? root.getElementById(id) : document.getElementById(id))?.textContent) || '')
      .map(text => text.replace(/\s+/g, ' ').trim())
      .filter(Boolean)
      .join(' ');
  };
  const labelFrom = (e, seen) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    const root = typeof e.getRootNode === 'function' ? e.getRootNode() : document;
    const byId = id => (root.getElementById ? root.getElementById(id) : document.getElementById(id));
    const referenced = (e.getAttribute('aria-labelledby') || '')
      .split(/\s+/)
      .map(id => labelFrom(byId(id), seen))
      .filter(Boolean)
      .join(' ');
    return (
      referenced ||
      e.getAttribute('aria-label') ||
      [...(e.labels || [])]
        .map(l => labelFrom(l, seen))
        .filter(Boolean)
        .join(' ') ||
      (['button', 'submit', 'reset'].includes(e.type) ? e.value : '') ||
      e.getAttribute('alt') ||
      (e.tagName === 'INPUT'
        ? ''
        : [...e.childNodes]
            .map(n =>
              n.nodeType === 3
                ? n.textContent
                : n.nodeType === 1 && !CODE.has(n.tagName) && n.getAttribute('aria-hidden') !== 'true'
                  ? labelFrom(n, seen)
                  : '',
            )
            .join(' ')
            .trim()) ||
      e.getAttribute('title') ||
      e.getAttribute('placeholder') ||
      ''
    );
  };

  const labelOf = memo(e => labelFrom(e, new Set()));

  const ARIA_ROLES = [
    'button',
    'link',
    'checkbox',
    'radio',
    'switch',
    'tab',
    'menuitem',
    'menuitemradio',
    'option',
    'gridcell',
    'combobox',
    'textbox',
    'searchbox',
    'spinbutton',
  ];
  // jQuery UI's datepicker prev/next are anchors with no href, appended to <body> once the field is focused:
  // they carry no destination, only a click handler, so they need naming here to be walked at all.
  const SELECTOR =
    'a[href],a[data-handler][data-event="click"],a[onclick],a[role="button"],' +
    'button,input,textarea,select,summary,label,[contenteditable="true"],[draggable="true"],[ondrop],' +
    ARIA_ROLES.map(role => `[role="${role}"]`).join(',');
  // A page says an element it styled or scripted is pressable without any role: a declared click handler, or the
  // pointer cursor. The cursor inherits, so only the element it starts on counts, and an element inside a control or
  // holding one leaves the click to that control.
  const pressable = memo(e => {
    const view = e.ownerDocument.defaultView;
    const starts =
      e.hasAttribute('onclick') ||
      (view.getComputedStyle(e).cursor === 'pointer' &&
        (!e.parentElement || view.getComputedStyle(e.parentElement).cursor !== 'pointer'));
    return starts && !e.parentElement?.closest(SELECTOR) && !e.querySelector(SELECTOR);
  });
  // Types whose value is a calendar date, not free text: the field writer must produce the type's ISO shape
  // and the native value setter, not typed keystrokes, is what a picker widget actually commits.
  const DATE_TYPES = ['date', 'datetime-local', 'month', 'week', 'time'];

  const roleOf = e => {
    if (sourceOf(e) !== e) return sourceOf(e).type === 'file' ? 'textbox' : sourceOf(e).type;
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
    // A bare draggable element or a declared drop target has no ARIA role of its own, yet it is a real
    // element to act on. Name it generic rather than null so the control walk does not drop it before the
    // drag it offers can be seen.
    if (e.getAttribute('draggable') === 'true' || e.hasAttribute('ondrop')) return 'generic';
    return pressable(e) ? 'button' : null;
  };

  const submitSemantics = memo(e => {
    const form = e.form;
    const explicit = ['BUTTON', 'INPUT'].includes(e.tagName) && ['submit', 'image'].includes(e.type);
    const implicit = [
      'text',
      'search',
      'url',
      'tel',
      'email',
      'password',
      'date',
      'month',
      'week',
      'time',
      'datetime-local',
      'number',
    ];
    if (!form || (!explicit && (e.tagName !== 'INPUT' || !implicit.includes(e.type)))) return null;
    const submit = explicit ? e : [...form.getRootNode().querySelectorAll('button,input')].find(
      c => c.form === form && (c.type === 'submit' || c.type === 'image'),
    );
    if (submit?.matches(':disabled')) return null;
    if (!submit && [...form.elements].filter(c => c.tagName === 'INPUT' && implicit.includes(c.type)).length > 1)
      return null;
    return JSON.stringify({
      label: submit ? labelOf(submit) : labelOf(form),
      method: submit?.getAttribute('formmethod') || form.method,
      action: submit?.getAttribute('formaction') || form.action,
      text: innerTextOf(form),
    });
  });

  const framePath = doc => {
    if (doc === document) return null;
    const owner = doc.defaultView.frameElement;
    return `${framePath(owner.ownerDocument) || 'root'}/${identity(owner)}`;
  };

  // Traverse light DOM plus any open shadow roots, and same-origin same-process nested iframes.
  let inaccessible = 0;
  function* walk(root) {
    // A document observer cannot see mutations inside shadow roots or child documents.
    registry.track(root);
    const listed = new Set(root.querySelectorAll(SELECTOR));
    for (const e of root.querySelectorAll('*')) {
      if (listed.has(e) || pressable(e)) yield e;
      if (e.shadowRoot) yield* walk(e.shadowRoot);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        const inner = childDocument(e);
        if (inner && inner.body && granted(inner)) yield* walk(inner);
        else inaccessible++;
      }
    }
  }

  // Guards and the page key go to Rust as digests: each comparison is equality, and the full guards cost more to
  // send than the snapshot takes to compute. Not cryptographic, and it needs not be: a page that wants a guard to
  // hold can simply leave its DOM alone.
  const digest = text => {
    let h1 = 0xdeadbeef,
      h2 = 0x41c6ce57;
    for (let i = 0; i < text.length; i++) {
      const c = text.charCodeAt(i);
      h1 = Math.imul(h1 ^ c, 2654435761);
      h2 = Math.imul(h2 ^ c, 1597334677);
    }
    h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909);
    h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909);
    return (h2 >>> 0).toString(16).padStart(8, '0') + (h1 >>> 0).toString(16).padStart(8, '0');
  };
  // Sibling controls share their card, so its text is hashed once per card rather than once per control.
  const scopeDigest = memo(scope => digest(innerTextOf(scope)?.slice(0, GUARD_TEXT_CHARS) || ''));
  registry.guard = e => {
    if (!e?.isConnected || !visible(e)) return null;
    const source = sourceOf(e);
    if (source !== e && (source.matches(':disabled') || source.closest('[aria-disabled="true"],[inert]'))) return null;
    const scope = e.closest('form,dialog,[role="dialog"],article,li,tr,[role="row"]') || e.parentElement;
    // An absent aria-disabled and "false" both mean enabled. Google Flights adds the "false" as it hydrates, and
    // the raw attribute turned the run's first click stale.
    return [
      identity(e),
      roleOf(e),
      labelOf(e),
      reveal(source),
      source.checked ?? null,
      e.selectedIndex ?? null,
      e.readOnly ?? null,
      e.matches(':disabled'),
      e.getAttribute('aria-disabled') === 'true',
      e.getAttribute('aria-expanded'),
      e.getAttribute('aria-checked'),
      e.getAttribute('aria-selected'),
      e.getAttribute('href'),
      scope ? scopeDigest(scope) : '',
      e.ownerDocument.location.origin,
      e.ownerDocument.defaultView.performance.timeOrigin,
      submitSemantics(e),
      source.getAttribute('name'),
      source.getAttribute('autocomplete'),
      identity(source),
    ];
  };
  registry.guardKey = e => {
    const guard = registry.guard(e);
    return guard && digest(JSON.stringify(guard));
  };

  const controls = [];
  // Controls with no accessible name of their own, whose label is an icon's or only their role.
  const unnamed = new Set();
  // A button is scoped too, so a submission can be told apart by the fields of the form it sends.
  const fieldScope = e => {
    if (!e.matches('input,textarea,select,button')) return null;
    const form = e.form || e.closest('form,fieldset,[role="form"]');
    if (form) return String(identity(form));
    // Script-built forms can use a local group of labelled fields without an HTML form owner.
    // Stop at document sections so a sidebar search never joins the content's fields.
    for (
      let p = e.parentElement;
      p && !p.matches('body,main,article,section,aside,nav,header,footer');
      p = p.parentElement
    ) {
      if (p.querySelectorAll('input:not([type=hidden]),textarea,select').length > 1) return String(identity(p));
    }
    return null;
  };
  for (const e of walk(document)) {
    if (!safe(e) || !visible(e) || e.matches(':disabled') || e.closest('[aria-disabled="true"],[inert]')) continue;
    const source = sourceOf(e);
    if (
      source !== e &&
      (visible(source) || source.matches(':disabled') || source.closest('[aria-disabled="true"],[inert]'))
    )
      continue;
    const r = e.getBoundingClientRect(),
      x = r.x + r.width / 2,
      y = r.y + r.height / 2,
      rname = roleOf(e);
    if (!rname || r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth) continue;
    if (rname === 'gridcell' && e.querySelector('button,[role="button"]')) continue;
    // A button-styled link offers the same label twice, but its inner button owns every useful click point.
    if (rname === 'link') {
      // Most links hold no button, and querying every link for every control was the walk's slowest step.
      const nested = e.querySelector('button') ? e.querySelectorAll(SELECTOR) : [];
      const button = nested.length === 1 ? nested[0] : null;
      if (button?.tagName === 'BUTTON' && visible(button) && !button.matches(':disabled') &&
          !button.closest('[aria-disabled="true"],[inert]') && labelOf(button) === labelOf(e)) continue;
    }

    const id = identity(e);
    registry.controls.add(e);
    const named = labelOf(e);
    if (!named) unnamed.add(id);
    const base = {
      id,
      role: rname,
      label: named || iconLabel(e) || describedBy(e) || rname,
      offscreen: y < 0 || y >= innerHeight,
      distance: y < 0 || y >= innerHeight ? 1 + Math.abs(y - innerHeight / 2) : 0,
      sensitive: secret(source),
      input_type: source.type || null,
      input_name: source.getAttribute('name') || null,
      autocomplete: source.getAttribute('autocomplete') || null,
      frame_origin: e.ownerDocument.location.origin,
      frame_path: framePath(e.ownerDocument),
      form_id: fieldScope(source),
      submit_semantics: submitSemantics(e),
    };
    if (rname === 'link' && e.href) {
      const u = new URL(e.href, location.href);
      base.href = u.origin === location.origin ? u.pathname + u.search : u.host + u.pathname;
      // The page's own word for "the next page of this list", which survives an icon label or another language.
      if (e.rel && e.rel.split(/\s+/).includes('next')) base.next_page = true;
      // The page's own word that this link saves a file, which a task ruling out downloads has to see before a click.
      if (e.hasAttribute('download')) base.download = true;
    }
    for (const key of ['checked', 'selected', 'expanded']) {
      const value = e.getAttribute('aria-' + key);
      if (value !== null) base[key] = value === 'true';
    }
    if (['checkbox', 'radio'].includes(source.type)) base.checked = source.checked;
    // What the page says about the field. A custom widget marks the element the user sees, not the input
    // underneath it, so both are asked.
    if (source.required || [e, source].some(n => n.getAttribute('aria-required') === 'true')) base.required = true;
    if ([e, source].some(n => n.getAttribute('aria-invalid') === 'true')) base.invalid = true;
    if (e.tagName === 'SELECT') {
      base.operations = ['select'];
      base.options = [...e.options].filter(o => !o.disabled && !o.closest('optgroup[disabled]')).map(o => o.label);
      base.value = [...e.selectedOptions].map(o => o.label).join(', ');
    } else if (source.type === 'file') {
      base.operations = ['upload'];
      base.value = null;
    } else {
      const editable =
        !e.readOnly &&
        e.getAttribute('aria-readonly') !== 'true' &&
        (['textbox', 'searchbox', 'spinbutton'].includes(rname) ||
          (rname === 'combobox' && ['INPUT', 'TEXTAREA'].includes(e.tagName)));
      // reveal() is null for <li>, <progress> and <meter>, whose numeric `value`s are not field contents.
      base.value = reveal(source) ?? (e.isContentEditable || rname === 'combobox' ? e.innerText.trim() : null);
      base.operations = editable ? ['fill', 'click', 'enter'] : ['click'];
      // Only an element that names itself draggable is offered as a drag source: `draggable` reports true for
      // every <a> and <img> by default, which would offer the whole page as draggable.
      if (e.getAttribute('draggable') === 'true') base.operations.push('drag');
    }
    controls.push(base);
  }

  // Controls that read the same cannot be told apart by their label, and a hint naming the right one
  // ("Add to cart under Sauce Labs Backpack") has nothing to match on. Give each twin the widest ancestor
  // that holds it and none of its twins, named by that ancestor's heading or its own first line of text.
  // Only labels that actually collide pay for it.
  const HEADINGS = 'h1,h2,h3,h4,h5,h6,[role="heading"],legend,caption,th,dt,summary';
  const firstLine = text =>
    (text || '')
      .split('\n')
      .map(s => s.replace(/\s+/g, ' ').trim())
      .find(Boolean) || '';
  // A label tied to no control titles what follows it, as a heading would: a blog post's "Date Picker 3" is one,
  // and the post's first heading ("Data Entry Form") named that picker's Submit when only headings counted.
  // A hidden title names nothing on screen: innerText still returns a `hidden` label's text.
  const titles = memo(scope =>
    [...scope.querySelectorAll(`${HEADINGS},label`)].filter(
      e =>
        (e.localName !== 'label' || !e.control) && e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }),
    ));
  // A table's header cell names its own row, and a caption its own table, never what sits outside them: a jQuery UI
  // datepicker's Next button was named "Su", its weekday header, and every month's Next read the same.
  const reaches = (title, element) =>
    !['th', 'caption'].includes(title.localName) || title.parentElement.contains(element);
  // The title nearest before the element names its section; the scope's first title is the fallback, and the
  // only answer when `nearest` is false.
  const sectionOf = (scope, element, label, nearest = true) => {
    let first = '',
      before = '';
    for (const title of titles(scope)) {
      const text = firstLine(innerTextOf(title));
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
    const lines = (innerTextOf(scope) || '')
      .split('\n')
      .map(line => line.replace(/\s+/g, ' ').trim())
      .filter(line => line && !repeated.has(line));
    return firstLine(lines.map(line => (label ? ` ${line} `.split(` ${label} `).join(' ') : line)).join('\n'));
  };
  const contextOf = (element, holding, label, nearest = true) => {
    // The widest twin-free ancestor is the card, row or section the twins repeat over. A narrower one names
    // the button's own wrapper, which on a shop is its price rather than the product it belongs to.
    let scope = null;
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      if (holding.get(e) > 1) break;
      scope = e;
      // A lone open reply has no form twin nearby; crossing its record picks a different commenter's name.
      if (e.matches('article,li,[role="listitem"]') ||
        (e.matches('tr,[role="row"]') && e.querySelector('input,textarea,select,[role="textbox"]'))) break;
    }
    return scope ? nameOf(scope, element, label, nearest) : '';
  };
  // Content a stylesheet shows only under the pointer (`.card:hover .caption`) is out of reach of every other
  // operation. An element whose hover rule would reveal something now hidden is offered as a hover target.
  const docs = [];
  const addDoc = doc => {
    if (!doc?.body || !granted(doc) || docs.includes(doc)) return;
    docs.push(doc);
    for (const frame of doc.querySelectorAll('iframe,frame')) {
      try {
        addDoc(frame.contentDocument);
      } catch {
        /* A cross-origin frame is observed in its own session. */
      }
    }
  };
  addDoc(document);
  // Parsing every stylesheet each step is the slow part on a large site, so a document's hover rules are kept
  // until its rule counts change (a new sheet, or rules a script inserted).
  registry.hoverRules ||= new WeakMap();
  const revealsIn = doc => {
    const sheets = [];
    for (const sheet of doc.styleSheets) {
      try {
        sheets.push(sheet.cssRules);
      } catch {
        /* A cross-origin stylesheet cannot be read. */
      }
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
          const pair = [
            (m[1] + m[2]).replaceAll(':hover', '').trim(),
            `:scope ${m[3]} ${m[4].replaceAll(':hover', '')}`,
          ];
          reveals.set(pair.join('\n'), pair);
        }
      }
    };
    for (const rules of sheets) collect(rules);
    const found = [...reveals.values()];
    registry.hoverRules.set(doc, { version, reveals: found });
    return found;
  };
  const offered = new Set(controls.map(c => registry.nodes.get(c.id)));
  for (const doc of docs) {
    for (const [base, tail] of revealsIn(doc)) {
      let hosts = [];
      try {
        hosts = doc.querySelectorAll(base);
      } catch {
        continue;
      }
      for (const e of hosts) {
        // A link or button's own hover rule restyles it or shows decoration; offering HOVER there as well as
        // CLICK only gives the choice another way to be wrong.
        if (offered.has(e) || e === doc.body || e === doc.documentElement || !visible(e)) continue;
        const r = e.getBoundingClientRect(),
          x = r.x + r.width / 2,
          y = r.y + r.height / 2;
        if (r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth || r.height > innerHeight) continue;
        let hidden = false;
        try {
          hidden = [...e.querySelectorAll(tail)].some(
            t => !visible(t) && (t.textContent.trim() || t.querySelector('img,a')),
          );
        } catch {
          continue;
        }
        // Removing the active hover target renumbers its identical siblings and sends the next choice
        // to a different element. Keep it offered while the pointer still reveals its content.
        if (!hidden && !e.matches(':hover')) continue;
        const image = e.querySelector('img[alt]');
        const label =
          e.getAttribute('aria-label') ||
          image?.getAttribute('alt') ||
          firstLine(e.innerText) ||
          e.getAttribute('title') ||
          e.tagName.toLowerCase();
        const role =
          e.getAttribute('role') ||
          { FIGURE: 'figure', LI: 'listitem', IMG: 'img', TR: 'row', TD: 'cell' }[e.tagName] ||
          'group';
        const c = {
          id: identity(e),
          role,
          label,
          offscreen: y < 0 || y >= innerHeight,
          distance: y < 0 || y >= innerHeight ? 1 + Math.abs(y - innerHeight / 2) : 0,
          sensitive: false,
          input_type: null,
          frame_origin: e.ownerDocument.location.origin,
          frame_path: framePath(e.ownerDocument),
          submit_semantics: null,
          value: null,
          operations: ['hover'],
        };
        controls.push(c);
        offered.add(e);
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
    // How many twins each ancestor holds. One that holds two is past the card, and counting once beats asking
    // every ancestor of every twin whether it contains each of the others.
    const holding = new Map();
    for (const twin of twins) {
      for (let e = twin.parentElement; e; e = e.parentElement) holding.set(e, (holding.get(e) || 0) + 1);
    }
    const nearest = group.map(c => contextOf(registry.nodes.get(c.id), holding, c.label));
    group.forEach((c, i) => {
      // Cards that each end in an "Options" heading share their nearest title, which would name every twin
      // alike; a twin whose nearest title another twin shares is named by its card's first title instead.
      const shared = nearest.indexOf(nearest[i]) !== nearest.lastIndexOf(nearest[i]);
      const context = shared ? contextOf(registry.nodes.get(c.id), holding, c.label, false) : nearest[i];
      if (context) c.context = context;
    });
    // Options that are direct children of one list share every ancestor, and their aria-label hides the text
    // inside them, so "Springfield" twice read alike even when one says "City in Illinois" and the other
    // "State park in Oregon". A twin with no context is told apart by what it shows besides its label, when
    // that differs.
    // Inline children join into one run in innerText, so each text node is read as a line of its own.
    const ownOf = (element, label) => {
      const lines = [];
      const walker = element.ownerDocument.createTreeWalker(element, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const line = node.data.replace(/\s+/g, ' ').trim();
        // A caption a hover rule reveals is not shown, so it cannot tell twins apart.
        if (!line || line === label || repeated.has(line)) continue;
        if (!node.parentElement?.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) continue;
        lines.push(` ${line} `.split(` ${label} `).join(' '));
      }
      return firstLine(lines.join('\n'));
    };
    const own = group.map(c => (c.context ? '' : ownOf(registry.nodes.get(c.id), c.label)));
    if (new Set(own.filter(Boolean)).size > 1 || (own.some(Boolean) && group.some(c => c.context))) {
      group.forEach((c, i) => {
        if (own[i]) c.context = own[i];
      });
    }
    // Twins whose surroundings name none of them, like a row of identical avatars, still differ by position.
    if (group.every(c => !c.context)) {
      twins.sort((a, b) => (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
      for (const c of group) {
        c.context = `${twins.indexOf(registry.nodes.get(c.id)) + 1} of ${twins.length}`;
      }
    }
  }

  // An unnamed control the page draws once per card, a trash icon beside each image, is a twin only while another
  // card shows the same icon: one left alone, or drawn by another icon set, would name no card and leave the choice
  // guessing which image it removes. It is named by the card that holds it, or failing one by the nearest ancestor
  // that says anything besides it.
  // A card is an item, a row, or an element the page repeats beside itself with the same tag and class.
  const isCard = e =>
    e.matches('article,li,[role="listitem"],tr,[role="row"]') ||
    (!!e.getAttribute('class') &&
      [...(e.parentElement?.children || [])].some(
        s => s !== e && s.localName === e.localName && s.getAttribute('class') === e.getAttribute('class'),
      ));
  const nearestName = (element, label) => {
    let first = '';
    for (let e = element.parentElement, path = element; e && e !== e.ownerDocument.body; path = e, e = e.parentElement) {
      first ||= nameOf(e, element, label, true);
      if (!isCard(e)) continue;
      // An action row's own caption ("Actions") is the first text the climb meets and names no item, so the card
      // is named by what it holds outside the row that holds the control.
      const aria = (e.getAttribute('aria-label') || '').trim();
      const outside = [...e.children]
        .filter(child => child !== path)
        .map(child => firstLine(innerTextOf(child)))
        .find(line => line && line !== label && !repeated.has(line));
      return (aria !== label && aria) || outside || first;
    }
    return first;
  };
  for (const c of controls) {
    if (!c.context && unnamed.has(c.id)) {
      const context = nearestName(registry.nodes.get(c.id), c.label);
      if (context) c.context = context;
    }
  }

  // A revealed caption needs its actual container association, not merely the most recent action.
  registry.hoverContexts = new WeakMap();
  // Hover targets are often every card of one list, so the sibling list is taken once per parent.
  const siblingsOf = memo(parent => [...(parent?.children || [])]);
  for (const c of controls) {
    if (!c.operations.includes('hover')) continue;
    const element = registry.nodes.get(c.id);
    registry.hoverContexts.set(element, {
      control: {
        role: c.role, label: c.label,
        context: /^\d+ of \d+$/.test(c.context || '') ? c.context : null,
      },
      labelAttributes: [element.getAttribute('role'), element.getAttribute('aria-label'),
        element.getAttribute('title'), element.querySelector('img[alt]')?.getAttribute('alt') || null],
      text: innerTextOf(element),
      parent: element.parentNode,
      siblings: siblingsOf(element.parentNode),
    });
  }

  // A control's own label rarely says which section it belongs to when a page repeats a form or widget: unlike a
  // colliding label, a uniquely named one never reaches the loop above, so the nearest ancestor that names a section
  // is attached here on its own, without the twin-avoidance `contextOf` needs to keep a card from naming its neighbour.
  const nearestHeading = (element, label) => {
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      const aria = (e.getAttribute('aria-label') || '').trim();
      if (aria && aria !== label) return aria;
      const section = sectionOf(e, element, label);
      if (section) return section;
    }
    return '';
  };
  for (const c of controls) {
    if (!c.context) {
      const context = nearestHeading(registry.nodes.get(c.id), c.label);
      if (context) c.context = context;
    }
  }

  // Flattened text loses empty cells: a calendar day then has no weekday, and the chooser picked a Tuesday
  // for the first Friday. Keep the table's column names beside its controls, including uniquely named ones.
  const columns = new Map();
  for (const c of controls) {
    const cell = registry.nodes.get(c.id).closest('td,th');
    const table = cell?.closest('table');
    if (!table) continue;
    if (!columns.has(table)) {
      const positions = new Map(),
        headers = [],
        occupied = [];
      const rows = [...table.rows];
      rows.forEach((row, r) => {
        let col = 0;
        for (const cell of row.cells) {
          while (occupied[col] > r) col++;
          const end = col + cell.colSpan;
          positions.set(cell, [col, end, r]);
          const remaining = row.parentElement.rows.length - row.sectionRowIndex;
          for (let i = col; i < end; i++) occupied[i] = r + Math.min(cell.rowSpan || remaining, remaining);
          if (
            cell.tagName === 'TH' &&
            !['row', 'rowgroup'].includes(cell.scope) &&
            (cell.scope ||
              row.parentElement.tagName === 'THEAD' ||
              (r === 0 && [...row.cells].every(c => c.tagName === 'TH')))
          )
            headers.push(cell);
          col = end;
        }
      });
      const names = new Map();
      for (const [cell, [start, end, row]] of positions) {
        const associated = cell.headers
          ? cell.headers.split(/\s+/).map(id => cell.ownerDocument.getElementById(id))
          : headers.filter(header => {
              const [left, right, above] = positions.get(header);
              return above < row && left < end && right > start;
            });
        const labels = associated
          .filter(header => header && visible(header))
          .map(
            header =>
              header.getAttribute('abbr') ||
              header.getAttribute('title') ||
              header.querySelector('[title]')?.getAttribute('title') ||
              labelOf(header),
          );
        names.set(cell, [...new Set(labels.filter(Boolean))].join(' / '));
      }
      columns.set(table, names);
    }
    const column = columns.get(table).get(cell);
    if (column) c.context = [c.context, `column: ${column}`].filter(Boolean).join('; ');
  }

  controls.sort((a, b) => a.distance - b.distance);

  // Text in a same-process child document is on screen as much as the parent's: a frameset page has no text of
  // its own at all. Each child is read in its own coordinates, shifted by where its frame sits.
  const words = [];
  // Visible text the viewport does not show, counted in characters: a read takes it in, and a page whose answer
  // sits further down a long list is not one to scroll through screen by screen.
  let offscreen_text = 0;
  const readFrames = (doc, dx, dy, read) => {
    for (const frame of doc.querySelectorAll('iframe,frame')) {
      const inner = childDocument(frame);
      if (!inner || !granted(inner)) continue;
      const f = frame.getBoundingClientRect();
      read(inner, dx + f.left + frame.clientLeft, dy + f.top + frame.clientTop);
    }
  };
  // Size, the cheapest test, goes first.
  const readText = (doc, dx, dy) => {
    if (!doc.body) return;
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
    const range = doc.createRange();
    let node;
    while ((node = walker.nextNode())) {
      const value = node.textContent.trim(),
        parent = node.parentElement;
      if (!value || !parent) continue;
      range.selectNodeContents(node);
      const r = range.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0 || parent.closest('script,style,noscript,template') || !visible(parent)) {
        continue;
      }
      const top = r.top + dy,
        left = r.left + dx;
      if (top + r.height > 0 && top < innerHeight && left + r.width > 0 && left < innerWidth) {
        words.push(value);
      } else {
        offscreen_text += value.length;
      }
    }
    readFrames(doc, dx, dy, readText);
  };
  readText(document, 0, 0);
  const viewport_text = words.join('\n');

  // Page state carried by pixels: a chart, a picture, a drawing or a video is evidence the page text and the
  // control list cannot hold, and the verifier needs the screenshot for it. Only what the viewport shows counts,
  // because that is what a screenshot captures. Geometry is read in the parent's coordinates the way readText reads
  // text, so a frame's image counts where the frame itself sits on screen.
  const VISUAL = 'img,canvas,video,svg,picture,figure,[role="img"]';
  let visual_content = 0;
  const readVisual = (doc, dx, dy) => {
    if (visual_content) return;
    for (const e of doc.querySelectorAll(VISUAL)) {
      if (e.closest('[aria-hidden="true"],[inert]') || !visible(e)) continue;
      const r = e.getBoundingClientRect();
      const top = r.top + dy,
        left = r.left + dx;
      if (r.width <= 0 || r.height <= 0) continue;
      if (top + r.height <= 0 || top >= innerHeight || left + r.width <= 0 || left >= innerWidth) continue;
      visual_content = 1;
      return;
    }
    readFrames(doc, dx, dy, readVisual);
  };
  readVisual(document, 0, 0);

  const guards = {},
    retarget_keys = {};
  for (const c of controls) {
    const guard = registry.guard(registry.nodes.get(c.id));
    guards[c.id] = guard && digest(JSON.stringify(guard));
    // A redraw changes the node ids at either end of the guard. Everything between them, including the receiving
    // document's timeOrigin and form semantics, must survive before an action can follow it.
    retarget_keys[c.id] = guard && digest(JSON.stringify(guard.slice(1, -1)));
  }
  // Kept so a later check can ask whether any of these controls changed without sending the guards back.
  registry.observed = new Map(controls.map(c => [c.id, guards[c.id]]));
  const field_state = [...document.querySelectorAll('input,textarea,select')]
    .filter(safe)
    .map(e => [identity(e), reveal(e), e.checked, e.selectedIndex, e.disabled, e.readOnly]);
  // Compare meaning and identity for the page_key fingerprint; geometry is re-resolved just before input.
  const semantics = controls.map(({ distance, ...c }) => c);
  const page_key = digest(JSON.stringify([
    location.href,
    scrollX,
    scrollY,
    innerWidth,
    innerHeight,
    document.title,
    viewport_text,
    semantics,
    field_state,
  ]));

  const result = {
    url: location.href,
    title: document.title,
    viewport_text,
    offscreen_text,
    visual_content,
    document_key: String(performance.timeOrigin),
    controls: semantics,
    page_key,
    guards,
    retarget_keys,
    inaccessible_frames: inaccessible,
  };
  return result;
})
