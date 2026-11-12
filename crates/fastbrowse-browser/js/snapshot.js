// Ported from jev-ultrafast (MIT): jev_ultrafast/snapshot.js.
// Atomic single-evaluate observation of one frame's controls, viewport text, and a freshness fingerprint.
// Adapted to fastbrowse's Control shape (role + operation set, not per-kind actions) and to traverse
// open shadow roots. Runs once per frame session (main frame or an OOPIF); CdpPage merges frames.
// biome-ignore format: session.rs appends mode arguments to this parenthesized expression.
((mode, allowedOrigins = null, own = {}, base = null) => {
  // A caller can reuse an unchanged read or request deltas against its baseline. Plain `snapshot` and `glance`
  // always return the full page, so existing callers keep their original contract.
  const delta = mode.endsWith('-delta');
  const reuse = delta || mode.endsWith('-reuse');
  if (reuse) mode = mode.slice(0, -6);
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
  registry.nativeCancelAnimationFrame ||= window.cancelAnimationFrame.bind(window);
  // A click can schedule its first write after the next frame. Only work descended from the current action's input
  // holds a settle: unrelated page timers must not charge an idle click the whole deadline, and a loop an earlier
  // action started (a poll that reschedules itself, an animation frame chain) belongs to that action, so it must not
  // hold every later one. `action` numbers the run's actions: the first input after a read opens the next one.
  if (!registry.pendingWork) {
    registry.pendingWork = new Set();
    registry.action = 0;
    // The action whose input the running task descends from, or null. It lasts to the next task, so the promise
    // reactions and observer callbacks the task queued count as its work too. `turnInFrame` says the task is a frame
    // callback.
    registry.turn = null;
    registry.turnInFrame = false;
    let turns = 0;
    registry.inputTurn = (action, inFrame = false) => {
      registry.turn = action;
      registry.turnInFrame = inFrame;
      const current = ++turns;
      registry.nativeSetTimeout(() => {
        if (current === turns) registry.turn = null;
      }, 0);
    };
    // Whether the current action has work that runs before `deadline`, a `performance.now()` time. A timer cannot run
    // before its delay, so one due later cannot change what a settle bounded by `deadline` reads. A frame callback that
    // asks for the next frame is the page drawing, which never has to end: it holds only while `drawing`, which the
    // settle answers as it does for mutations, by its quiet window.
    registry.pendingBefore = (deadline, drawing) => {
      for (const work of registry.pendingWork)
        if (work.action === registry.action && work.due <= deadline && (drawing || !work.nextFrame)) return true;
      return false;
    };
    // `delayed` says whether the second argument is a delay: a frame callback is due at the next frame.
    const wrap = (schedule, cancel, delayed) => {
      const pending = new Map();
      const start = (callback, ...args) => {
        if (registry.turn === null || typeof callback !== 'function') return schedule(callback, ...args);
        // Web IDL converts a timer delay to a signed long. Convert once: an object's valueOf can change the page
        // or return another delay on a second call, so tracking and Chrome must receive the same number.
        if (delayed && args.length) args[0] = +args[0] | 0;
        const work = {
          action: registry.turn,
          due: performance.now() + (delayed ? Math.max(0, args[0] || 0) : 0),
          nextFrame: !delayed && registry.turnInFrame,
        };
        registry.pendingWork.add(work);
        let id;
        try {
          id = schedule(function (...values) {
            pending.delete(id);
            registry.pendingWork.delete(work);
            registry.inputTurn(work.action, !delayed);
            return Reflect.apply(callback, this, values);
          }, ...args);
        } catch (error) {
          registry.pendingWork.delete(work);
          throw error;
        }
        pending.set(id, work);
        return id;
      };
      const stop = id => {
        const key = delayed ? +id | 0 : +id >>> 0;
        registry.pendingWork.delete(pending.get(key));
        pending.delete(key);
        return cancel(key);
      };
      return [start, stop];
    };
    [window.setTimeout, window.clearTimeout] = wrap(registry.nativeSetTimeout, registry.nativeClearTimeout, true);
    // HTML timers share one handle map, so clearInterval can cancel a setTimeout too.
    window.clearInterval = window.clearTimeout;
    [window.requestAnimationFrame, window.cancelAnimationFrame] = wrap(
      registry.nativeRequestAnimationFrame,
      registry.nativeCancelAnimationFrame,
      false,
    );
  }
  // What was offered to be acted on; `ids` also names hover targets and fingerprinted nodes.
  registry.controls ||= new WeakSet();
  // A change counter, so a read can tell later whether the page it described has moved since. Everything here can
  // change what a read would report without rewriting a node: pointer entry reveals a `:hover` rule, a font or image
  // that finishes loading shifts the layout under it, a document observer sees neither a shadow root nor a child
  // document, and a sheet inserted through the CSSOM changes what is shown with no record to observe.
  if (!registry.revTracking) {
    registry.revTracking = true;
    registry.rev = 0;
    const bumped = () => registry.rev++;
    for (const type of [
      'pointermove',
      'pointerover',
      'pointerout',
      'mouseover',
      'mouseout',
      'mouseenter',
      'mouseleave',
      'input',
      'change',
      'reset',
      'focusin',
      'focusout',
      'scroll',
      'wheel',
      'resize',
      'load',
      'error',
      'play',
      'playing',
      'pause',
      'ended',
      'seeked',
      'timeupdate',
      'ratechange',
      'volumechange',
    ])
      addEventListener(type, bumped, true);
    new MutationObserver(bumped).observe(document, {
      subtree: true,
      characterData: true,
      childList: true,
      attributes: true,
    });
    // A short digest of a long text, so a fingerprint is kept and compared without holding the text itself.
    const hash = text => {
      let h = 0x811c9dc5;
      for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 16777619);
      return `${text.length}:${h >>> 0}`;
    };
    // The CSSOM edits a rule in place (`rule.style.display = 'none'`) with no record to observe and no rule added, so
    // every readable rule is fingerprinted by its text. A sheet another origin serves is not readable here, and its
    // rules are not what changed ours.
    registry.sheets = () => {
      const parts = [];
      for (const sheet of [...document.styleSheets, ...(document.adoptedStyleSheets || [])]) {
        parts.push(`${sheet.disabled}|${sheet.media?.mediaText}`);
        try {
          for (const rule of sheet.cssRules) parts.push(rule.cssText);
        } catch {
          parts.push('opaque');
        }
      }
      return hash(parts.join('\n'));
    };
    // A form control's live state is a property, not an attribute: `input.value = ...`, `checkbox.checked = true` and
    // `setCustomValidity(...)` change what a read reports with no event and no record to observe.
    registry.live = () => {
      const parts = [];
      for (const e of document.querySelectorAll('input,textarea,select,button')) {
        const chosen = e.type === 'select-multiple' ? [...e.selectedOptions].map(o => o.index).join() : e.selectedIndex;
        parts.push(JSON.stringify([e.value, e.checked, e.indeterminate, chosen, e.validationMessage]));
      }
      return hash(parts.join('\n'));
    };
    // Each kept read is numbered, so a caller answered "unchanged" can tell whether the read it holds is that one.
    registry.reads = 0;
  }
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
  // Elements a script gave a pointer listener, such as a Vue click handler on a bare span: the page's only sign that
  // it is pressed. Chrome's own `getEventListeners` reads them, but it exists only in an evaluate with the command line
  // API, so that evaluate (this mode, once per document) keeps it in `own`, the installed function's closure, where the
  // page cannot reach it. A look then asks it only about the elements it weighs (`pressable`), live, so a removed
  // listener takes its control away. A look before it is kept throws `own.unread`, which page/mod.rs answers with this
  // mode. A page that defines a global of that name answers for its own elements, as its cursor and `onclick` do.
  if (mode === 'listeners') {
    const read = globalThis.getEventListeners;
    own.listeners = typeof read === 'function' ? read : null;
    return 0;
  }
  if ((mode === 'snapshot' || mode === 'glance') && !('listeners' in own))
    throw new Error(own.unread);
  const POINTER = ['click', 'mousedown', 'pointerdown', 'mouseup', 'pointerup'];
  // The root element and the body hear clicks on the whole page, as the document does, so neither counts.
  const listened = e => {
    if (!own.listeners || e === e.ownerDocument.documentElement || e === e.ownerDocument.body) return false;
    let listeners = null;
    try {
      listeners = own.listeners(e);
    } catch {}
    return Boolean(listeners) && POINTER.some(type => Array.isArray(listeners[type]) && listeners[type].length);
  };
  // Text the page rewrites by itself, such as a player's clock or a countdown, says nothing about which item a control
  // acts on: a Pause beside a ticking remaining time was refused as stale on every one of 52 clicks. Text that changed
  // between two looks with no input in between is the page's own; text that changes after a run's own click, scroll
  // or typing still counts. The run's input arrives as trusted events, which is how the page tells the two apart.
  if (!registry.selfUpdating) {
    registry.selfUpdating = new Set();
    registry.changed = new Set();
    registry.quiet = false;
    // When the document last moved and when the run last touched it, as `performance.now()` reads them. A settle
    // resolves from these rather than from re-reading the page: the observer is the page's own report that it is still
    // writing, so one evaluation waits on it instead of being sent again until two reads agree. The action's own input
    // is stamped apart from its mutations, so an action that changes nothing is not charged a quiet window it has no
    // reason to wait out.
    registry.lastMutation = performance.now();
    registry.lastInput = 0;
    const input = event => {
      // Filling a field dispatches input and change, but a page's synthetic click is background work.
      if (!event.isTrusted && event.type !== 'input' && event.type !== 'change') return;
      // `quiet` is set by a read and cleared by input, so the first input since a read is a new action's.
      if (registry.quiet) registry.action++;
      registry.inputTurn(registry.action);
      registry.quiet = false;
      registry.lastInput = performance.now();
    };
    // Chrome can deliver release in a later task than press, after the press's input turn has ended.
    for (const type of [
      'pointerdown', 'pointerup', 'mousedown', 'mouseup', 'click', 'auxclick', 'dblclick',
      'keydown', 'keyup', 'keypress', 'submit', 'input', 'change', 'wheel', 'scroll',
    ]) {
      addEventListener(type, input, true);
    }
    new MutationObserver(records => {
      registry.lastMutation = performance.now();
      for (const r of records) {
        // An attribute change moves the page but rewrites no text, so it stamps the clock above and is not a changed
        // text node.
        if (r.type === 'attributes') continue;
        const textOnly =
          r.type === 'characterData' || [...r.addedNodes, ...r.removedNodes].every(n => n.nodeType === Node.TEXT_NODE);
        const element = r.type === 'characterData' ? r.target.parentElement : r.target;
        if (textOnly && element) registry.changed.add(element);
      }
    }).observe(document, { subtree: true, characterData: true, childList: true, attributes: true });
  }
  if (mode !== 'glance') {
    if (registry.quiet) for (const e of registry.changed) registry.selfUpdating.add(e);
    for (const e of registry.selfUpdating) if (!e.isConnected) registry.selfUpdating.delete(e);
    registry.changed.clear();
    registry.quiet = true;
  }
  const clock = () => ({
    ready: document.readyState === 'interactive' || document.readyState === 'complete',
    hidden: document.hidden,
  });
  if (mode === 'fingerprint') {
    let hash = 2166136261;
    const hashText = text => {
      for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
    };
    // Players' clocks stay out of the hash: one that plays moves by itself, which must not make a control stale.
    // They are kept beside it, rounded as readMedia rounds them, for a Wait to tell a moving clock.
    const clocks = [];
    const include = root => {
      hashText(root.body?.innerText ?? root.textContent ?? '');
      // innerText omits shadow trees and child documents even when their content is visible.
      for (const e of root.querySelectorAll('*')) {
        // A panel can move without changing any text or the document's own scroll position.
        if ((e.scrollLeft !== 0 || e.scrollTop !== 0) && e.checkVisibility({checkVisibilityCSS: true}))
          hashText(JSON.stringify([e.scrollLeft, e.scrollTop]));
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
        // readMedia lists the players of documents only, not of shadow roots.
        if (root.nodeType === Node.DOCUMENT_NODE && (e.tagName === 'AUDIO' || e.tagName === 'VIDEO'))
          clocks.push(Math.round(e.currentTime * 10) / 10);
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
      media: clocks.join(','),
    };
  }
  if (!document.body) return null;
  // A page that has not moved since the read it last gave is not read again: the caller still holds that read, and
  // deriving it and sending it back is what an observation costs on a dense page. Anything that can change what a
  // read would say without rewriting a node is asked first, so a reuse is only a page that is provably still.
  const kept = registry.kept?.[mode];
  const still =
    reuse &&
    kept &&
    kept.mode === mode &&
    (!delta || (base?.document_key === kept.document_key && base?.read === kept.read)) &&
    kept.rev === registry.rev &&
    kept.live === registry.live() &&
    kept.sheets === registry.sheets() &&
    kept.href === location.href &&
    kept.title === document.title &&
    kept.scrollX === scrollX &&
    kept.scrollY === scrollY &&
    kept.innerWidth === innerWidth &&
    kept.innerHeight === innerHeight &&
    kept.hidden === document.hidden &&
    kept.readyState === document.readyState &&
    // Upgrading a form-associated custom element can change HTML label ownership without a DOM mutation.
    !kept.undefinedElements?.some(e => e.isConnected && e.matches(':defined')) &&
    // A transition or animation in flight moves what is on screen between one look and the next, so a read taken
    // while one runs does not stand.
    document.getAnimations().length === 0;
  if (still)
    return {
      unchanged: true,
      url: kept.url,
      title: kept.title,
      document_key: kept.document_key,
      page_key: kept.page_key,
      read: kept.read,
    };
  // Taken before the read, so a change made while it runs leaves the kept read unmatched rather than reused.
  const readRev = registry.rev;
  const readLive = registry.live();
  const readSheets = registry.sheets();
  const identity = e => {
    if (!registry.ids.has(e)) registry.ids.set(e, registry.next++);
    const id = registry.ids.get(e);
    registry.nodes.set(id, e);
    return id;
  };
  for (const [id, e] of registry.nodes) if (!e.isConnected) {
    registry.nodes.delete(id);
    registry.scrollAxes?.delete(id);
  }
  // The DOM cannot change while this script runs, so a label, a text or a section title read once is reused for
  // the rest of it. `registry.guard` reads them again in a later script, against a page that may have changed
  // since, so the reuse ends as soon as this one does, whether it returns or throws.
  const caches = [];
  let reading = true;
  registry.nativeQueueMicrotask ||= window.queueMicrotask.bind(window);
  registry.nativeQueueMicrotask(() => {
    reading = false;
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
  // These queries use only element names and attributes, never live pseudo-classes or layout. A mutation clears
  // its subtree's and ancestors' indexes, leaving unrelated cards reusable. A full read builds its indexes anew.
  own.queryTrees ||= new WeakMap();
  own.domVersions ||= new WeakMap();
  own.structureVersions ||= new WeakMap();
  if (!delta || !base || base.document_key !== own.snapshots?.[mode]?.document_key ||
      base.read !== own.snapshots?.[mode]?.read) {
    own.queryResults = new WeakMap();
    own.names = new WeakMap();
  }
  own.queryResults ||= new WeakMap();
  own.names ||= new WeakMap();
  const flushedTrees = new Set();
  const invalidateQueries = (records, root) => {
    const bumped = new Set();
    for (const record of records) {
      for (let node = record.target; node && !bumped.has(node); node = node.parentNode) {
        bumped.add(node);
        own.queryResults.delete(node);
        own.domVersions.set(node, (own.domVersions.get(node) || 0) + 1);
      }
      for (const added of record.addedNodes || []) {
        if (added.nodeType !== 1) continue;
        // A removed subtree can be edited while no observer watches it, then inserted somewhere else.
        for (const node of [added, ...added.querySelectorAll('*')]) {
          own.queryResults.delete(node);
          own.domVersions.set(node, (own.domVersions.get(node) || 0) + 1);
        }
      }
      if (record.type === 'childList' && [...record.addedNodes, ...record.removedNodes].some(n => n.nodeType === 1))
        own.structureVersions.set(root, (own.structureVersions.get(root) || 0) + 1);
    }
  };
  const flushTree = scope => {
    const root = scope.getRootNode();
    let tree = own.queryTrees.get(root);
    if (!tree) {
      const observer = new MutationObserver(records => invalidateQueries(records, root));
      observer.observe(root, { subtree: true, childList: true, attributes: true, characterData: true });
      tree = { observer, undefinedElements: null, structure: null };
      own.queryTrees.set(root, tree);
    }
    if (!flushedTrees.has(root)) {
      invalidateQueries(tree.observer.takeRecords(), root);
      const structure = own.structureVersions.get(root) || 0;
      if (tree.structure !== structure || tree.undefinedElements === null) {
        tree.undefinedElements = [...root.querySelectorAll(':not(:defined)')];
        tree.structure = structure;
      } else {
        tree.undefinedElements = tree.undefinedElements.filter(node => !node.matches(':defined'));
      }
      if (root === document) own.undefinedElements = tree.undefinedElements;
      flushedTrees.add(root);
    }
    return root;
  };
  const descendants = (scope, selector) => {
    // A guard can run in a later script, before observer callbacks have delivered. It always queries live state.
    if (!reading) return scope.querySelectorAll(selector);
    const root = flushTree(scope);
    let entry = own.queryResults.get(scope);
    // A detached subtree can acquire different children before entering a newly observed shadow root.
    if (!entry || entry.root !== root) {
      entry = { root, queries: new Map() };
      own.queryResults.set(scope, entry);
    }
    if (!entry.queries.has(selector)) entry.queries.set(selector, scope.querySelectorAll(selector));
    return entry.queries.get(selector);
  };
  flushTree(document);
  const readUndefinedElements = own.undefinedElements;
  const innerTextOf = memo(e => e.innerText);

  const safe = e => e.type !== 'hidden';
  // Password and agent-typed secret values never leave the page: only their length is observed.
  const secret = e => e.type === 'password' || e.dataset?.fastbrowseSecret === '1';
  const reveal = e => (typeof e.value !== 'string' ? null : secret(e) ? '•'.repeat(e.value.length) : e.value);
  // What a select has chosen. selectedIndex names only the first chosen option, so a list that allows several gives
  // every chosen index: a later option chosen or taken out changes it.
  const chosen = e =>
    e.multiple && e.tagName === 'SELECT' ? [...e.selectedOptions].map(o => o.index) : (e.selectedIndex ?? null);
  // Tag pickers keep the field they replace on the page, clipped to nothing, and checkVisibility() ignores clip: a
  // control whose own clip or clip-path leaves no area is not there for a person. A hidden checkbox, radio or file
  // input is still reached through its label. Text is still read, as a page states facts in visually hidden text.
  const clippedAway = e => {
    const style = e.ownerDocument.defaultView.getComputedStyle(e);
    const rect = /^rect\((.+)\)$/.exec(style.clip);
    const inset = /^inset\(([^)]*?)(?:\s+round\s.*)?\)$/.exec(style.clipPath);
    // Almost every element has neither, and its box is not read.
    if (!rect && !inset) return false;
    const { width, height } = e.getBoundingClientRect();
    if (rect && ['absolute', 'fixed'].includes(style.position)) {
      // An `auto` side is the border box's own edge.
      const edges = [0, width, height, 0];
      const [top, right, bottom, left] = rect[1]
        .split(/[\s,]+/)
        .map((value, side) => (value === 'auto' ? edges[side] : parseFloat(value)));
      if (![top, right, bottom, left].some(Number.isNaN) && (bottom <= top || right <= left)) return true;
    }
    if (!inset) return false;
    const sides = inset[1].trim().split(/\s+/);
    const [top, right = top, bottom = top, left = right] = sides;
    const length = (value, size) =>
      value.endsWith('%')
        ? (parseFloat(value) / 100) * size
        : value.endsWith('px') || value === '0'
          ? parseFloat(value)
          : NaN;
    const across = length(left, width) + length(right, width),
      down = length(top, height) + length(bottom, height);
    return (across > 0 && across >= width) || (down > 0 && down >= height);
  };
  // A control that fades in under the pointer starts its opacity transition at zero, and the observation taken right
  // after the hover would otherwise see only that start. A CSS transition's last keyframe is where it is going, also
  // after a reversal, which restarts it as a new transition. A keyframe animation's last keyframe gives no direction
  // when it alternates or repeats, so only transitions count: anything else is NaN.
  const fadingTo = t =>
    t instanceof t.effect.target.ownerDocument.defaultView.CSSTransition &&
    !t.effect.pseudoElement &&
    t.transitionProperty === 'opacity' &&
    t.playState === 'running' &&
    t.playbackRate > 0 &&
    Number.isFinite(t.effect.getComputedTiming().endTime)
      ? parseFloat(t.effect.getKeyframes().at(-1)?.opacity)
      : NaN;
  // Whether `e` or an ancestor, across shadow roots, passes `test`.
  const onChain = (e, test) => {
    for (let a = e; a; a = a.parentElement ?? a.getRootNode().host) if (test(a)) return true;
    return false;
  };
  // Asked only of an element whose opacity check failed. One that ignores the pointer while it fades is not offered,
  // as what it covers would take the click.
  const fadingIn = e =>
    e.checkVisibility({ checkVisibilityCSS: true }) &&
    e.ownerDocument.defaultView.getComputedStyle(e).pointerEvents !== 'none' &&
    !onChain(
      e,
      a =>
        a.ownerDocument.defaultView.getComputedStyle(a).opacity === '0' &&
        !a.getAnimations().some(t => fadingTo(t) > 0),
    );
  // What is fading out has gone for a person though its opacity is still above zero, as the controls of a card the
  // pointer has just left are.
  const fadingOut = memo(doc => new Set(doc.getAnimations().flatMap(t => (fadingTo(t) === 0 ? [t.effect.target] : []))));
  const leaving = e => {
    const out = fadingOut(e.ownerDocument);
    return out.size > 0 && onChain(e, a => out.has(a));
  };
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
      (!e.checkVisibility({ checkOpacity: !choice, checkVisibilityCSS: true }) && (choice || !fadingIn(e))) ||
      leaving(e) ||
      (choice && !shownThrough(e)) ||
      (clippedAway(e) && (e.matches(SELECTOR) || pressable(e) || focusableText(e)))
    )
      return false;
    if (!choice) return true;
    const r = e.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && !e.matches(':disabled') && !e.closest('[aria-disabled="true"]');
  });
  registry.visible = visible;
  // Styled checkboxes and radios often hide the native input. Its visible label is the click
  // target, but the input still owns the checked/disabled state and must participate in freshness.
  // A control whose click opened a file chooser stands for the file input behind it, which can be hidden and unlabelled.
  // An upload dialog's button may make that input in script and never put it on the page, holding it by reference
  // to read its files, so only an input that was on the page when its chooser opened and has left it since is gone.
  const sourceOf = (registry.sourceOf = e =>
    registry.chooser?.opener === e && (registry.chooser.input.isConnected || !registry.chooser.connected)
      ? registry.chooser.input
      : e.tagName === 'LABEL' && ['checkbox', 'radio', 'file'].includes(e.control?.type)
        ? e.control
        : e);

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
    for (const icon of descendants(e, 'svg,i,span,[role="img"]')) {
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
  // An icon-only control with no name and no glyph often still says what it does in its own class: a `.delete-button`
  // beside an `.edit-button` on a card's hover-revealed controls. Only a class that calls itself a button counts, so
  // layout and framework classes never name a control.
  const CONTROL_WORD = /^(?:button|btn|icon)$/;
  // Style, size and structure words that name no action.
  const CLASS_STYLE = new Set([
    'icon', 'ico', 'base', 'root', 'primary', 'secondary', 'tertiary', 'default', 'small', 'large', 'medium', 'sm',
    'md', 'lg', 'xl', 'xs', 'outline', 'outlined', 'contained', 'ghost', 'flat', 'text', 'link', 'plain', 'round',
    'rounded', 'circle', 'square', 'pill', 'block', 'group', 'wrapper', 'container', 'inner', 'outer', 'main',
    'action', 'actions', 'ui', 'has', 'with', 'no', 'is', 'js', 'el', 'component', 'toggle', 'floating', 'fab',
  ]);
  const classLabel = e => {
    const names = new Set();
    for (const token of e.classList) {
      // A BEM modifier is dropped and a CSS-modules hash is split off: `Gallery_deleteButton__x7Y2a`.
      for (const piece of token.split(/--.*$|_+/)) {
        if (!/^[A-Za-z][A-Za-z-]{0,39}$/.test(piece)) continue;
        const words = piece
          .replace(/([a-z])([A-Z])/g, '$1-$2')
          .toLowerCase()
          .split('-')
          .filter(Boolean);
        if (words.length < 2) continue;
        let stem;
        if (CONTROL_WORD.test(words.at(-1))) stem = words.slice(0, -1);
        else if (/^(?:button|btn)$/.test(words[0])) stem = words.slice(1);
        else continue;
        stem = stem.filter(word => word.length > 1 && !CLASS_STYLE.has(word));
        if (stem.length && stem.length <= 3) names.add(stem.join(' '));
      }
    }
    return names.size === 1 ? [...names][0] : '';
  };
  // The label a control is offered under when it has no name, short of its bare role. The guard reads it too, so a
  // `.delete-button` turned into an `.edit-button` between observing and acting is stale.
  const fallbackLabel = e => iconLabel(e) || describedBy(e) || classLabel(e);
  // A name taken from content reads only what is shown: a card's hidden "Report fraud item" button named the card.
  // A hidden element named through aria-labelledby is read whole, since it is hidden only to serve as the label.
  // An svg's <title> never renders, yet it is the graphic's name.
  let nameReads = null;
  const shownNow = memo(e => e.localName === 'title' || e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }));
  const shown = e => {
    const value = shownNow(e);
    nameReads?.shown.set(e, value);
    return value;
  };
  const nameValue = e => {
    const value = e.value;
    nameReads?.values.set(e, value);
    return value;
  };
  const FIELD_CONTAINERS = new Set(['combobox', 'listbox']);
  // The one field an element holds, when it holds exactly one. A hidden input or one under `hidden` is no field a
  // person fills, and an element holding two says nothing about which of them a name on it is for.
  const soleField = e => {
    const fields = [...descendants(e, 'input:not([type=hidden]),select,textarea')].filter(
      f => !f.parentElement.closest('[hidden]'),
    );
    return fields.length === 1 ? fields[0] : null;
  };
  // A label's `for` can name a widget's wrapper rather than its field: a form library's `<label for="categories">`
  // over react-select points at the wrapper div, which takes the id, while the combobox input inside gets a generated
  // one. HTML ties a label to no control unless its target is labelable, yet the page has said which element the
  // label names, and when that element holds exactly one field the label names that field. Without this the
  // Categories field was offered nameless and named by its chips, and the label, tied to no control, titled the
  // checkboxes and Save button below it as the Categories section (#838).
  const labelTarget = memo(label => {
    if (label.control) return label.control;
    const id = label.htmlFor;
    if (!id) return null;
    const root = label.getRootNode();
    const target = root.getElementById ? root.getElementById(id) : document.getElementById(id);
    return target ? soleField(target) : null;
  });
  // The fields each tree's wrapper-naming labels name, built from its `label[for]` elements so the cost grows with
  // the labels, not with every unlabelled field times its id'd ancestors: per-ancestor queries took about a second
  // per observation on a 2000-row inline-edit grid. Each tree is indexed once and kept for as long as this snapshot's
  // `registry.guard` is: the redraw watch reads every observed control's guard on each mutation batch, and searching
  // the labels again there cost a second per batch. The guard then reads the labels this snapshot tied to a field,
  // so their text or visibility changing still changes it.
  const borrowedIndex = new WeakMap();
  const borrowedLabels = root => {
    if (!borrowedIndex.has(root)) {
      const byField = new Map();
      for (const label of root.querySelectorAll('label[for]')) {
        const field = label.control ? null : labelTarget(label);
        if (field) byField.set(field, [...(byField.get(field) || []), label]);
      }
      borrowedIndex.set(root, byField);
    }
    return borrowedIndex.get(root);
  };
  const wrapperLabels = e =>
    e.labels?.length || !e.matches('input,select,textarea') ? [] : borrowedLabels(e.getRootNode()).get(e) || [];
  const labelsFor = e => {
    const labels = [...(e.labels || []), ...wrapperLabels(e)];
    nameReads?.labels.set(e, labels);
    return labels;
  };
  const referencedNode = (root, id) => root.getElementById ? root.getElementById(id) : document.getElementById(id);
  const labelFrom = (e, seen) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    nameReads?.nodes.add(e);
    const root = typeof e.getRootNode === 'function' ? e.getRootNode() : document;
    const byId = id => {
      if (!id) return null;
      const node = referencedNode(root, id);
      nameReads?.references.push([root, id, node]);
      return node;
    };
    const referenced = (e.getAttribute('aria-labelledby') || '')
      .split(/\s+/)
      .map(id => labelFrom(byId(id), seen))
      .filter(Boolean)
      .join(' ');
    return (
      referenced ||
      e.getAttribute('aria-label') ||
      // A hidden label names nothing, as accname skips hidden nodes outside aria-labelledby: two tabbed forms reusing
      // one id made the hidden tab's label one of the shown field's `labels`, and the field was named twice (#921).
      labelsFor(e)
        .filter(l => shown(l) || !shown(e))
        .map(l => labelFrom(l, seen))
        .filter(Boolean)
        .join(' ') ||
      (['button', 'submit', 'reset'].includes(e.type) ? nameValue(e) : '') ||
      e.getAttribute('alt') ||
      // A field's text is its value, not its name: a list-style select shows every option, and a combobox widget its
      // chosen chips, remove buttons and open options. Accname never names these from content either. A textarea's
      // child text is the value it was served with: a settings page's 8 KB description became the field's label, and
      // the gate read that text as what a click on the field would do (#885).
      (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.tagName) || FIELD_CONTAINERS.has(e.getAttribute('role'))
        ? ''
        : [...e.childNodes]
            .map(n =>
              n.nodeType === 3
                ? n.textContent
                : n.nodeType === 1 &&
                    !CODE.has(n.tagName) &&
                    n.getAttribute('aria-hidden') !== 'true' &&
                    (shown(n) || !shown(e))
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

  // An ARIA 1.1 combobox wraps the field it fronts, and the page labels that field rather than the wrapper. A wrapper
  // holding two fields, such as a hidden select beside a search input, says nothing about which one names it.
  const wrappedField = e =>
    !e || e.matches('input,select,textarea') || !FIELD_CONTAINERS.has(e.getAttribute('role')) ? null : soleField(e);
  // A name is flattened as accname flattens it, whitespace runs to one space: a template's line breaks and indentation
  // inside a label reached callers as `Username               (required)` (#921).
  const contentName = e => {
    if (!e) return '';
    if (!reading) return labelFrom(e, new Set()) || labelFrom(wrappedField(e), new Set());
    const root = flushTree(e);
    const kept = own.names.get(e);
    if (kept && kept.root === root &&
        kept.nodes.every(([node, version]) => (own.domVersions.get(node) || 0) === version) &&
        kept.references.every(([tree, id, node]) => referencedNode(tree, id) === node) &&
        kept.labels.every(([node, labels]) => {
          const now = labelsFor(node);
          return now.length === labels.length && now.every((label, i) => label === labels[i]);
        }) &&
        (!kept.needsWrapped || wrappedField(e) === kept.wrapped) &&
        kept.shown.every(([node, value]) => shown(node) === value) &&
        kept.values.every(([node, value]) => node.value === value)) return kept.text;
    const reads = { nodes: new Set(), references: [], labels: new Map(), shown: new Map(), values: new Map() };
    nameReads = reads;
    let text, wrapped = null, needsWrapped = false;
    try {
      text = labelFrom(e, new Set());
      if (!text) {
        needsWrapped = true;
        wrapped = wrappedField(e);
        text = labelFrom(wrapped, new Set());
      }
    } finally {
      nameReads = null;
    }
    own.names.set(e, {
      root, text, wrapped, needsWrapped,
      references: reads.references, labels: [...reads.labels],
      nodes: [...reads.nodes].map(node => [node, own.domVersions.get(node) || 0]),
      shown: [...reads.shown], values: [...reads.values],
    });
    return text;
  };
  const labelOf = memo(e =>
    (
      (e && !e.getAttribute('aria-label') && !e.getAttribute('aria-labelledby') && focusableText(e)) ||
      contentName(e)
    )
      .replace(/\s+/g, ' ')
      .trim(),
  );

  // A page says an element it styled or scripted is pressable without any role: a declared click handler, a pointer
  // listener a script added, or the pointer cursor. The cursor inherits, so only the element it starts on counts, and
  // an element inside a control or holding one leaves the click to that control.
  const pressable = memo(e => {
    const view = e.ownerDocument.defaultView;
    const declared =
      e.hasAttribute('onclick') ||
      typeof e.onclick === 'function' ||
      (view.getComputedStyle(e).cursor === 'pointer' &&
        (!e.parentElement || view.getComputedStyle(e.parentElement).cursor !== 'pointer'));
    if (!(declared || listened(e)) || e.parentElement?.closest(SELECTOR) || [...descendants(e, SELECTOR)].some(visible))
      return false;
    // A listener can sit on a container to hear its children's clicks, as React's does on its root, so one holding a
    // pressable element leaves the click to it.
    return declared || ![...descendants(e, '*')].some(c => pressable(c) && visible(c));
  });
  // A script can make a focusable label its menu trigger without a role, href or pointer cursor. Its label may
  // be wrapped in a child, and hidden menu links must not hide the trigger that reveals them. A container holding
  // a visible control leaves input to that control instead.
  const focusableText = memo(e => {
    if (e.getAttribute('tabindex') === null || Number(e.getAttribute('tabindex')) < 0) return null;
    if (e.matches(SELECTOR) || e.parentElement?.closest(SELECTOR)) return null;
    if ([...descendants(e, SELECTOR)].some(visible)) return null;
    const text = (innerTextOf(e) || '').replace(/\s+/g, ' ').trim();
    return text || null;
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
    // A focusable label is pressed through the page's own listener, as a button is.
    if (focusableText(e)) return 'button';
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
    // A page can make any of these properties return something other than text. Coursera's React app left an
    // HTMLInputElement where a method string belongs and `JSON.stringify` threw on React's circular fiber, which ended
    // the run as a browser error; a value that is not text is no more use to a caller than none.
    const shown = v => (typeof v === 'string' || typeof v === 'number' ? String(v) : '');
    return JSON.stringify({
      label: shown(submit ? labelOf(submit) : labelOf(form)),
      method: shown(submit?.getAttribute('formmethod') || form.method),
      action: shown(submit?.getAttribute('formaction') || form.action),
      text: shown(innerTextOf(form)),
    });
  });

  const framePath = doc => {
    if (doc === document) return null;
    const owner = doc.defaultView.frameElement;
    return `${framePath(owner.ownerDocument) || 'root'}/${identity(owner)}`;
  };

  // Traverse light DOM plus any open shadow roots, and same-origin same-process nested iframes.
  let inaccessible = 0;
  // A document observer sees neither a shadow root nor a child document, so a read that went into either cannot be
  // told later whether it still holds, and is not reused.
  let deep = false;
  function* walk(root) {
    // A document observer cannot see mutations inside shadow roots or child documents.
    const listed = new Set(root.querySelectorAll(SELECTOR));
    for (const e of root.querySelectorAll('*')) {
      if (listed.has(e) || pressable(e) || focusableText(e)) yield e;
      if (e.shadowRoot) {
        deep = true;
        yield* walk(e.shadowRoot);
      }
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        const inner = childDocument(e);
        if (inner && inner.body && granted(inner)) {
          deep = true;
          yield* walk(inner);
        } else inaccessible++;
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
  const scopeDigest = memo(scope => {
    let text = innerTextOf(scope) || '';
    for (const e of registry.selfUpdating) if (scope.contains(e)) text = text.replace(innerTextOf(e) || '', '');
    return digest(text.slice(0, GUARD_TEXT_CHARS));
  });
  registry.guard = (e, id) => {
    if (!e?.isConnected || !visible(e)) return null;
    const axis = registry.scrollAxes?.get(id);
    if (axis) return [identity(e), 'scroll', axis,
      e.getAttribute('aria-label'), e.getAttribute('aria-labelledby'), e.getAttribute('title'),
      e.ownerDocument.location.origin, e.ownerDocument.defaultView.performance.timeOrigin, identity(e)];
    const source = sourceOf(e);
    if (source !== e && (source.matches(':disabled') || source.closest('[aria-disabled="true"],[inert]'))) return null;
    const scope = e.closest('form,dialog,[role="dialog"],article,li,tr,[role="row"]') || e.parentElement;
    // An absent aria-disabled and "false" both mean enabled. Google Flights adds the "false" as it hydrates, and
    // the raw attribute turned the run's first click stale.
    return [
      identity(e),
      roleOf(e),
      labelOf(e) || fallbackLabel(e),
      reveal(source),
      source.checked ?? null,
      chosen(e),
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
  registry.guardKey = (e, id) => {
    const guard = registry.guard(e, id);
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
      if (descendants(p, 'input:not([type=hidden]),textarea,select').length > 1) return String(identity(p));
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
      const nested = e.querySelector('button') ? descendants(e, SELECTOR) : [];
      const button = nested.length === 1 ? nested[0] : null;
      if (button?.tagName === 'BUTTON' && visible(button) && !button.matches(':disabled') &&
          !button.closest('[aria-disabled="true"],[inert]') && labelOf(button) === labelOf(e)) continue;
    }

    const id = identity(e);
    if (mode !== 'glance') registry.controls.add(e);
    const named = labelOf(e);
    if (!named) unnamed.add(id);
    const base = {
      id,
      role: rname,
      label: named || fallbackLabel(e) || rname,
      offscreen: y < 0 || y >= innerHeight,
      distance: y < 0 || y >= innerHeight ? 1 + Math.abs(y - innerHeight / 2) : 0,
      sensitive: secret(source),
      // The root's own active element, so a field inside a shadow root reads as focused where the document names
      // only its host.
      focused: e.getRootNode().activeElement === e,
      input_type: source.type || null,
      input_name: source.getAttribute('name') || null,
      max_length: source.maxLength > 0 ? source.maxLength : null,
      autocomplete: source.getAttribute('autocomplete') || null,
      frame_origin: e.ownerDocument.location.origin,
      frame_path: framePath(e.ownerDocument),
      form_id: fieldScope(source),
      submit_semantics: submitSemantics(e),
    };
    if (rname === 'link' && e.href) {
      const u = new URL(e.href, location.href);
      // Another origin's link keeps its whole address, as capture.js writes it: a recipe or a follow joins the href
      // to this page, which would turn host and path into a path on this site and drop the query (#830).
      base.href = u.origin === location.origin ? u.pathname + u.search : u.href;
      // The page's own word for "the next page of this list", which survives an icon label or another language:
      // `rel="next"`, or the pager markup most sites use instead, a `next` class on the link or its list item inside a
      // pagination container (#750), one named by `pagin` or `pager`, as "page" alone is a layout class. A `next`
      // class alone also marks carousels, wizards and next-article links. Only a same-origin link is a pager: its href
      // is a path on this site, where another origin's is an absolute address.
      const pager = el => el && el.classList && el.classList.contains('next');
      const paging = el =>
        el.tagName === 'NAV' ||
        /pagin|pager/i.test(typeof el.className === 'string' ? el.className : '') ||
        /pagin|pager/i.test(el.getAttribute('aria-label') || '');
      const container = () => {
        for (let el = e.parentElement; el; el = el.parentElement) if (paging(el)) return true;
        return false;
      };
      if (
        u.origin === location.origin &&
        ((e.rel && e.rel.split(/\s+/).includes('next')) ||
          ((pager(e) || (pager(e.parentElement) && e.parentElement.tagName === 'LI')) && container()))
      )
        base.next_page = true;
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
    // Native validation can refuse a form without adding any ARIA attribute or page text. The browser holds that
    // against a field only once it was edited or its form was submitted, as `:user-invalid`: an empty required field a
    // page has just opened or reset is not refused, and told it was, a run that had submitted a wizard filled it again.
    const refused = source.willValidate === true && source.validity?.valid === false && source.matches(':user-invalid');
    if ([e, source].some(n => n.getAttribute('aria-invalid') === 'true') || refused) base.invalid = true;
    // Why the browser refused it, in its own words: a refused submit shows that only in a bubble the page text
    // lacks, and a run that saw only `invalid` refilled the field the task named instead of the empty one.
    if (refused && source.validationMessage) base.validation_message = source.validationMessage;
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
    [...descendants(scope, `${HEADINGS},label`)].filter(
      e =>
        (e.localName !== 'label' || !labelTarget(e)) && e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }),
    ));
  // A card is an item, a row, or an element the page repeats beside itself with the same tag and class.
  // Each parent's children counted by tag and class once: asking every child about all its siblings is quadratic
  // in a list of thousands, and every element of the page is asked whether it is a card.
  const twinKey = e => `${e.localName} ${e.getAttribute('class')}`;
  const twinCounts = memo(parent => {
    const counts = new Map();
    for (const child of parent?.children || []) counts.set(twinKey(child), (counts.get(twinKey(child)) || 0) + 1);
    return counts;
  });
  const isCard = memo(e =>
    e.matches('article,li,[role="listitem"],tr,[role="row"]') ||
    (!!e.getAttribute('class') && twinCounts(e.parentElement).get(twinKey(e)) > 1));
  // A landmark or section owns its headings. A weather panel beside a search form must not name the search
  // field just because both sit in the same app shell.
  const isolatesTitle = memo(e =>
    isCard(e) || e.matches('aside,nav,main,section,form,fieldset,dialog,search,[role="group"],[role="search"],[role="alertdialog"],[role="complementary"],[role="navigation"],[role="main"],[role="region"],[role="form"],[role="dialog"]'));
  // A table's header cell names its own row, a caption its own table and a legend its own fieldset, never what sits
  // outside them: a form's "Customer name" was named after the "Pizza Size" fieldset below it, and a jQuery UI
  // datepicker's Next button was named "Su", its weekday header, and every month's Next read the same. A title inside
  // a card names that card alone: a list's "More" button was named after the last product shown above it.
  const reaches = (title, element) => {
    if (['th', 'caption', 'legend'].includes(title.localName) && !title.parentElement.contains(element)) return false;
    for (let e = title.parentElement; e && !e.contains(element); e = e.parentElement) if (isolatesTitle(e)) return false;
    return true;
  };
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
  const controlByElement = new Map();
  const labelsByRole = new Map();
  const nameOf = (scope, element, label, nearest, own = null) => {
    const aria = (scope.getAttribute('aria-label') || '').trim();
    if (aria && aria !== label) return aria;
    const section = sectionOf(scope, element, label, nearest);
    if (section) return section;
    // A line that is another control of the same kind names that control, not the section: a calendar's day cells were
    // named "31" and "7" after their neighbours in the same week row, so a picker showing November was offered with
    // contexts that pointed at other days. A card's title keeps its line, since the control it names there is another
    // kind (a link inside a checkbox's label names the item). Past a repeated control's label, a datepicker's first
    // line is the month it shows. The label is cut out as whole words, since a day labelled "2" cut out of
    // "October 2026" left "October 0 6".
    const mine = controlByElement.get(element);
    const counts = labelsByRole.get(mine?.role);
    const peer = word => (counts?.get(word) || 0) > Number(mine?.label === word);
    // A table row's cells are one line of text, so a week row reads "26 27 28 29 30 1 2": a line made only of this
    // control's and its peers' labels names nothing but them.
    const onlyPeers = line => line.split(' ').every(word => word === label || peer(word));
    const lines = (innerTextOf(scope) || '')
      .split('\n')
      .map(line => line.replace(/\s+/g, ' ').trim())
      .filter(line => line && !repeated.has(line) && !peer(line) && !onlyPeers(line) && !own?.has(line));
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
  // until their text changes: editing a selector in place changes what a hover reveals without adding a rule.
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
    const selectors = [];
    const collect = rules => {
      for (const rule of rules) {
        if (rule.cssRules?.length) collect(rule.cssRules);
        if (rule.selectorText) selectors.push(rule.selectorText);
      }
    };
    for (const rules of sheets) collect(rules);
    const version = JSON.stringify(selectors);
    const cached = registry.hoverRules.get(doc);
    if (cached?.version === version) return cached.reveals;
    const reveals = new Map();
    for (const text of selectors) {
      for (const selector of text.split(',')) {
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
    const found = [...reveals.values()];
    registry.hoverRules.set(doc, { version, reveals: found });
    return found;
  };
  const offered = new Set(controls.map(c => registry.nodes.get(c.id)));
  // What carries page state in pixels: a chart, a picture, a drawing or a video.
  const VISUAL = 'img,canvas,video,svg,picture,figure,[role="img"]';
  // A page marks an icon beside a label aria-hidden: it decorates the words and carries no state of its own.
  const decorative = e => !!e.closest('[aria-hidden="true"],[inert]');
  // Where the pointer can rest: an element with a box inside the viewport's width and no taller than it.
  const pointerBox = e => {
    if (e === e.ownerDocument.body || e === e.ownerDocument.documentElement || !visible(e)) return null;
    const r = e.getBoundingClientRect(),
      x = r.x + r.width / 2,
      y = r.y + r.height / 2;
    return r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth || r.height > innerHeight ? null : y;
  };
  const offerHover = (e, y) => {
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
    controls.push({
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
    });
    offered.add(e);
  };
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
        if (offered.has(e)) continue;
        const y = pointerBox(e);
        if (y === null) continue;
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
        offerHover(e, y);
      }
    }
  }
  // A script can render a card's controls only while a real pointer is over it, from hover state no stylesheet
  // names: a media page's delete and reorder buttons were absent from the document until then, so nothing on
  // the card could be acted on and the run reported an image it could see as one it could not find. A card of a
  // list that shows a picture and holds nothing to act on is offered as a hover target, so the pointer can go
  // there and the next observation shows what appeared. A card of text alone, a paragraph or a calendar's
  // weekday row, is read rather than handled, and offering every one would bury the choice. An icon the page hides
  // from assistive technology is decoration, not a picture: GitHub's license badge (a law icon beside
  // "BSD-3-Clause") was offered as a hover target on every repository card, and the run hovered one instead of
  // opening the repository. Only the outermost such card is offered.
  const holding = new Set();
  for (const e of offered) for (let p = e?.parentElement; p && !holding.has(p); p = p.parentElement) holding.add(p);
  for (const doc of docs) {
    let host = null;
    for (const e of doc.body.querySelectorAll('*')) {
      if (host?.contains(e)) continue;
      if (offered.has(e) || holding.has(e) || !isCard(e)) continue;
      if (![...descendants(e, VISUAL)].some(v => !decorative(v))) continue;
      const y = pointerBox(e);
      if (y === null) continue;
      offerHover(e, y);
      host = e;
    }
  }

  const byLabel = new Map();
  // Context compares a line with the other controls' labels. Index those once, rather than scanning every control
  // for each card or calendar cell; counts preserve a label another control shares with this one.
  for (const c of controls) {
    const element = registry.nodes.get(c.id);
    if (!controlByElement.has(element)) controlByElement.set(element, c);
    if (!labelsByRole.has(c.role)) labelsByRole.set(c.role, new Map());
    const labels = labelsByRole.get(c.role);
    labels.set(c.label, (labels.get(c.label) || 0) + 1);
  }
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
  // An unnamed select or combobox shows its value, and its chips name no section it sits in.
  const nearestName = (element, label) => {
    const own =
      element.tagName === 'SELECT' || FIELD_CONTAINERS.has(element.getAttribute('role'))
        ? new Set((innerTextOf(element) || '').split('\n').map(line => line.replace(/\s+/g, ' ').trim()))
        : null;
    let first = '';
    for (let e = element.parentElement, path = element; e && e !== e.ownerDocument.body; path = e, e = e.parentElement) {
      first ||= nameOf(e, element, label, true, own);
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
  if (mode !== 'glance') {
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
  }

  // The titles a scope's own content can be named by: none sealed in a card below the scope, and a header cell,
  // caption or legend only in the element it heads. For anything outside the scope's child that holds the control,
  // this is what `reaches` decides, so it is worked out once per scope rather than once per control.
  const openTitles = memo(scope =>
    titles(scope).filter(title => {
      if (['th', 'caption', 'legend'].includes(title.localName) && title.parentElement !== scope) return false;
      for (let e = title.parentElement; e !== scope; e = e.parentElement) if (isolatesTitle(e)) return false;
      return true;
    }),
  );
  // A control's own label rarely says which section it belongs to when a page repeats a form or widget: unlike a
  // colliding label, a uniquely named one never reaches the loop above, so the nearest ancestor that names a section
  // is attached here on its own, without the twin-avoidance `contextOf` needs to keep a card from naming its neighbour.
  // Each ancestor is asked only for titles outside the one below it, whose own were already turned down; a title
  // inside the control is its own content, never the section it sits in.
  const nearestHeading = (element, label) => {
    for (let inner = element, e = element.parentElement; e && e !== e.ownerDocument.body; inner = e, e = e.parentElement) {
      const aria = (e.getAttribute('aria-label') || '').trim();
      if (aria && aria !== label) return aria;
      let first = '',
        before = '';
      for (const title of openTitles(e)) {
        if (inner.contains(title)) continue;
        const text = firstLine(innerTextOf(title));
        if (!text || text === label) continue;
        first ||= text;
        if (title.compareDocumentPosition(element) & Node.DOCUMENT_POSITION_FOLLOWING) before = text;
      }
      if (before || first) return before || first;
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

  // A wheel at the viewport centre misses panels and horizontal lists. Index the browser's actual scroll owners,
  // separately for each overflowing axis, so the chosen target determines which content moves.
  registry.scrollIds ||= new WeakMap();
  registry.scrollAxes ||= new Map();
  const scrollAreas = root => {
    for (const e of root.querySelectorAll('*')) {
      if (e.shadowRoot) scrollAreas(e.shadowRoot);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        const inner = childDocument(e);
        if (inner?.body && granted(inner)) scrollAreas(inner);
      }
      if (!safe(e) || !visible(e) || e.closest('[inert]')) continue;
      const view = e.ownerDocument.defaultView;
      const page = e === e.ownerDocument.scrollingElement;
      const overflow = {x: e.scrollWidth - e.clientWidth, y: e.scrollHeight - e.clientHeight};
      if (overflow.x <= 0 && overflow.y <= 0) continue;
      const style = view.getComputedStyle(e);
      const vertical = style.writingMode.startsWith('vertical') || style.writingMode.startsWith('sideways');
      let inlineReverse = style.direction === 'rtl';
      let blockReverse = vertical && style.writingMode.endsWith('-rl');
      if (style.display.includes('flex')) {
        if (style.flexDirection === 'row-reverse') inlineReverse = !inlineReverse;
        if (style.flexDirection === 'column-reverse') blockReverse = !blockReverse;
      }
      const named = element => element.hasAttribute('aria-label') || element.hasAttribute('aria-labelledby')
        ? labelOf(element) : element.getAttribute('title') || '';
      const frameNames = [];
      for (let doc = e.ownerDocument; doc !== document;) {
        const frame = doc.defaultView.frameElement;
        frameNames.unshift(named(frame) || doc.location.href);
        doc = frame.ownerDocument;
      }
      for (const axis of ['x', 'y']) {
        let behavior = axis === 'x' ? style.overflowX : style.overflowY;
        // With visible root overflow, CSS propagates the body's overflow to the viewport, including modal locks.
        if (page && behavior === 'visible' && e.ownerDocument.body) {
          const bodyStyle = view.getComputedStyle(e.ownerDocument.body);
          behavior = axis === 'x' ? bodyStyle.overflowX : bodyStyle.overflowY;
        }
        if (overflow[axis] <= 0 || !(page && behavior === 'visible') && !['auto', 'scroll', 'overlay'].includes(behavior)) continue;
        const position = axis === 'x' ? e.scrollLeft : e.scrollTop;
        // CSSOM View's overflow direction follows the block and inline flow, including reversed flex axes.
        const reverse = (axis === 'x') === vertical ? blockReverse : inlineReverse;
        const min = reverse ? -overflow[axis] : 0, max = reverse ? 0 : overflow[axis];
        const operations = [];
        if (position < max) operations.push('scroll');
        if (position > min) operations.push('scroll_up');
        if (!operations.length) continue;
        let ids = registry.scrollIds.get(e);
        if (!ids) registry.scrollIds.set(e, ids = {});
        const id = ids[axis] ||= registry.next++;
        registry.nodes.set(id, e);
        registry.scrollAxes.set(id, axis);
        const rect = e.getBoundingClientRect();
        controls.push({
          id, role: 'region', operations,
          label: `${axis === 'x' ? 'Horizontal' : 'Vertical'} scroll area: ${page ? 'Page' : named(e) || e.tagName.toLowerCase()}`,
          ...(frameNames.length ? {context: `Frame: ${frameNames.join(' / ')}`} : {}),
          frame_origin: e.ownerDocument.location.origin,
          frame_path: framePath(e.ownerDocument),
          offscreen: rect.bottom <= 0 || rect.top >= view.innerHeight,
          distance: rect.bottom <= 0 || rect.top >= view.innerHeight ? 1 + Math.abs(rect.top) : 0,
        });
      }
    }
  };
  scrollAreas(document);

  controls.sort((a, b) => a.distance - b.distance);

  // Text in a same-process child document is on screen as much as the parent's: a frameset page has no text of
  // its own at all. Each child is read in its own coordinates, shifted by where its frame sits.
  const words = [];
  // Visible text the viewport does not show. Jev judging only the screen took a list below the fold for a page with
  // nothing on it, and went past it unread.
  const offscreen = [];
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
        offscreen.push(value);
      }
    }
    readFrames(doc, dx, dy, readText);
  };
  readText(document, 0, 0);
  const viewport_text = words.join('\n');
  const offscreen_text = offscreen.join('\n');

  // Page state carried by pixels: a chart, a picture, a drawing or a video is evidence the page text and the
  // control list cannot hold, and the verifier needs the screenshot for it. Only what the viewport shows counts,
  // because that is what a screenshot captures. Geometry is read in the parent's coordinates the way readText reads
  // text, so a frame's image counts where the frame itself sits on screen.
  let visual_content = 0;
  const readVisual = (doc, dx, dy) => {
    if (visual_content) return;
    for (const e of doc.querySelectorAll(VISUAL)) {
      if (decorative(e) || !visible(e)) continue;
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

  // A player's state lives in the element, not the page's text: native controls draw their clock in shadow DOM, so a
  // run asked to confirm audio plays and then stops pressed Play and Pause until its step limit, unable to see either.
  // Every player counts, shown or not, since a hidden one can still be playing.
  const media = [];
  // Section headings only, as capture.js names players: a table's column header ("Comment") or a player's own
  // settings legend ("Font Family") named the players after them on Wikimedia Commons.
  const headingBefore = e => {
    let found = '';
    for (const h of e.ownerDocument.querySelectorAll('h1,h2,h3,h4,h5,h6')) {
      if (!(h.compareDocumentPosition(e) & Node.DOCUMENT_POSITION_FOLLOWING)) break;
      found = firstLine(innerTextOf(h)) || found;
    }
    return found;
  };
  const seconds = value => (Number.isFinite(value) ? Math.round(value * 10) / 10 : null);
  const readMedia = doc => {
    for (const e of doc.querySelectorAll('audio,video')) {
      media.push({
        kind: e.tagName.toLowerCase(),
        // Its children are fallback text for browsers without players, not its name.
        label: (e.hasAttribute('aria-label') || e.hasAttribute('aria-labelledby') ? labelOf(e) : e.title) || null,
        heading: headingBefore(e) || null,
        paused: e.paused,
        ended: e.ended,
        muted: e.muted,
        elapsed_seconds: seconds(e.currentTime) ?? 0,
        duration_seconds: seconds(e.duration),
      });
    }
    readFrames(doc, 0, 0, readMedia);
  };
  readMedia(document);

  const guards = {},
    retarget_keys = {};
  for (const c of controls) {
    const guard = registry.guard(registry.nodes.get(c.id), c.id);
    guards[c.id] = guard && digest(JSON.stringify(guard));
    // A redraw changes the node ids at either end of the guard. Everything between them, including the receiving
    // document's timeOrigin and form semantics, must survive before an action can follow it.
    retarget_keys[c.id] = guard && digest(JSON.stringify(guard.slice(1, -1)));
  }
  // Kept so a later check can ask whether any of these controls changed without sending the guards back.
  if (mode !== 'glance') registry.observed = new Map(controls.map(c => [c.id, guards[c.id]]));
  const field_state = [...document.querySelectorAll('input,textarea,select')]
    .filter(safe)
    .map(e => [identity(e), reveal(e), e.checked, chosen(e), e.disabled, e.readOnly]);
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
    controls.filter(c => registry.scrollAxes?.has(c.id)).map(c => {
      const e = registry.nodes.get(c.id);
      return [c.id, e.scrollLeft, e.scrollTop];
    }),
    // Playing or not changes the page; the clock advancing does not, or no key would hold until an action.
    media.map(({ elapsed_seconds, ...m }) => m),
  ]));

  // The open modal the rest of the page sits behind. A page marks what is under one aria-hidden or inert, so those
  // controls leave the observation while staying in the document: Google Flights' Stops filter took 145 controls
  // out and put them back on closing, and a run told each by name in every later step.
  const modals = [...document.querySelectorAll('dialog:modal,[aria-modal="true"]')].filter(
    e => !e.closest('[aria-hidden="true"],[inert]') && e.checkVisibility({ checkVisibilityCSS: true }),
  );
  // A dialog is named by aria-label or aria-labelledby, never by its content.
  const dialogName = e =>
    (
      e.getAttribute('aria-label') ||
      (e.getAttribute('aria-labelledby') || '')
        .split(/\s+/)
        .map(id => (id && e.ownerDocument.getElementById(id)?.innerText) || '')
        .join(' ')
    )
      .replace(/\s+/g, ' ')
      .trim();
  const modal = modals.length ? dialogName(modals.at(-1)) : null;

  // The read number names a transport baseline. Only shallow reads can answer unchanged: a document observer
  // cannot see mutations inside a shadow root or child document. Deep reads are still derived in full for deltas.
  const read = !deep || delta ? ++registry.reads : null;
  if (!deep) {
    registry.kept ||= {};
    registry.kept[mode] = {
      mode,
      read,
      rev: readRev,
      live: readLive,
      sheets: readSheets,
      href: location.href,
      title: document.title,
      scrollX,
      scrollY,
      innerWidth,
      innerHeight,
      hidden: document.hidden,
      readyState: document.readyState,
      undefinedElements: readUndefinedElements,
      url: location.href,
      document_key: String(performance.timeOrigin),
      page_key,
    };
  }
  const result = {
    url: location.href,
    title: document.title,
    viewport_text,
    offscreen_text,
    visual_content,
    media,
    document_key: String(performance.timeOrigin),
    controls: semantics,
    page_key,
    guards,
    retarget_keys,
    inaccessible_frames: inaccessible,
    read,
  };
  // Absent unless one is open, as on most pages: an unnamed modal is the empty string, not null.
  if (modal !== null) result.modal = modal;
  const previous = own.snapshots?.[mode];
  own.snapshots ||= {};
  own.snapshots[mode] = { read, document_key: result.document_key, controls: semantics, guards, retarget_keys };
  if (delta && read !== null && previous?.read !== null && previous?.document_key === result.document_key &&
      base?.document_key === previous.document_key && base?.read === previous.read) {
    // Layout, CSS and references can change controls outside a mutated subtree. Derive the whole read, then omit
    // only rows and guards that are exactly equal; action-time guards never read this transport baseline.
    const rows = new Map(previous.controls.map(c => [c.id, JSON.stringify(c)]));
    const changed = (now, before) => Object.fromEntries(Object.entries(now).filter(([id, value]) => value !== before[id]));
    const { controls, guards, retarget_keys, ...metadata } = result;
    return {
      ...metadata,
      control_delta: {
        base_read: previous.read,
        controls: controls.map(c => rows.get(c.id) === JSON.stringify(c) ? c.id : c),
        guards: changed(guards, previous.guards),
        retarget_keys: changed(retarget_keys, previous.retarget_keys),
      },
    };
  }
  return result;
})
