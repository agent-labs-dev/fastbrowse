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
  // The run's own cursor (cursor.js) goes into the page and out again around actions and screenshots. That is the run
  // drawing, not the page changing, so its host's records are left out of every observer below.
  const cursorHost = node => node.nodeType === Node.ELEMENT_NODE && node.hasAttribute('data-fastbrowse-cursor');
  // An attribute written with the value it already held moves nothing: `attributeOldValue` lets a record say so.
  const rewritten = r =>
    r.type === 'attributes' && r.oldValue !== null && r.oldValue === r.target.getAttributeNS(r.attributeNamespace, r.attributeName);
  const pageRecords = records =>
    records.filter(
      r =>
        !cursorHost(r.target) &&
        !rewritten(r) &&
        !(r.type === 'childList' && [...r.addedNodes, ...r.removedNodes].every(cursorHost)),
    );
  const POINTER = ['click', 'mousedown', 'pointerdown', 'mouseup', 'pointerup'];
  // Whether `read`, this document's `getEventListeners` (`listeners` below), finds a listener for one of `types` on `e`.
  const hears = (read, e, types = POINTER) => {
    let listeners = null;
    try {
      listeners = read?.(e);
    } catch {}
    return Boolean(listeners) && types.some(type => Array.isArray(listeners[type]) && listeners[type].length);
  };
  // A parent document's look asks this one, a frame inside it, about this document's elements (`readerOf` below). It
  // returns before anything else here runs, and hands out only that answer, never the function.
  if (mode === 'reader') return 'listeners' in own ? (own.reader ||= (e, types) => hears(own.listeners, e, types)) : null;
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
  const nativeChoice = e => e.tagName === 'INPUT' && ['checkbox', 'radio'].includes(nativeProperty(e, 'type'));
  const shownThrough = e => {
    for (let a = e.parentElement ?? e.getRootNode().host; a; a = a.parentElement ?? a.getRootNode().host)
      if (styleProperty(a, 'opacity') === '0') return false;
    return true;
  };
  const registry = (window.__fastbrowse ||= { ids: new WeakMap(), nodes: new Map(), next: 1 });
  // Browser rendering and live selector results can change without a DOM mutation. A saved read records only
  // the native facts it consumed; replaying those facts validates the same branches without deriving the result.
  let renderFacts = mode === 'snapshot' || mode === 'glance' ? new Map() : null;
  let renderInputs = renderFacts ? [] : null;
  registry.nativeQueueMicrotask ||= window.queueMicrotask.bind(window);
  registry.nativeQueueMicrotask(() => {
    renderFacts = null;
    renderInputs = null;
  });
  const textRanges = new WeakMap();
  const readRenderFact = (kind, target, args) => {
    switch (kind) {
      case 'style': return target.ownerDocument.defaultView.getComputedStyle(target)[args];
      case 'visible': return target?.checkVisibility(args);
      case 'box': return target.getBoundingClientRect();
      case 'textBox': {
        const doc = target.ownerDocument;
        let range = textRanges.get(doc);
        if (!range) textRanges.set(doc, range = doc.createRange());
        range.selectNodeContents(target);
        return range.getBoundingClientRect();
      }
      // Scroll affordances can change through CSS state or generated content without a DOM mutation.
      case 'scrollSize': return [target.scrollWidth, target.clientWidth, target.scrollHeight, target.clientHeight];
      case 'scrollPosition': return [target.scrollLeft, target.scrollTop];
      case 'scrollStyle': {
        const style = target.ownerDocument.defaultView.getComputedStyle(target);
        return [style.writingMode, style.direction, style.display,
          style.display.includes('flex') ? style.flexDirection : null, style.overflowX, style.overflowY];
      }
      // Both pseudo-elements in one fact: one cache entry and one recorded input per element instead of two. Their
      // other properties matter only when `content` draws a string, so a pseudo-element without one reads no more.
      case 'pseudo': {
        const view = target.ownerDocument.defaultView;
        return ['::before', '::after'].flatMap(pseudo => {
          const style = view.getComputedStyle(target, pseudo);
          const content = style.content;
          return content === 'none' || content === 'normal'
            ? [null, null, content, null, null]
            : [style.display, style.visibility, content, style.marginInlineStart, style.marginInlineEnd];
        });
      }
      case 'text': return target?.innerText;
      case 'query': return target.querySelectorAll(args);
      case 'stack': return target.elementsFromPoint(args[0], args[1]);
      case 'state': return target.matches(args);
      case 'active': return target.activeElement;
      case 'selected': return [...target.selectedOptions];
      case 'validity': return target.validity?.valid;
      case 'selection': return selectionOf(target);
      case 'prop': return target?.[args];
    }
  };
  const renderValue = (kind, value) =>
    kind === 'box' || kind === 'textBox'
      ? [value.x, value.y, value.width, value.height, value.left, value.top, value.right, value.bottom]
      : kind === 'query' || kind === 'stack' ? [...value] : value;
  const sameRenderValue = (left, right) =>
    Array.isArray(left) ? Array.isArray(right) && left.length === right.length &&
      left.every((value, i) => Object.is(value, right[i])) : Object.is(left, right);
  const boxFields = ['x', 'y', 'width', 'height', 'left', 'top', 'right', 'bottom'];
  const sameRenderFact = (kind, value, current) => {
    if (kind === 'box' || kind === 'textBox')
      return boxFields.every(
        (field, i) => Object.is(value[i], current[field]));
    if (kind === 'query' || kind === 'stack') return value.length === current.length && value.every((node, i) => node === current[i]);
    return sameRenderValue(value, current);
  };
  const renderFact = (kind, target, args = null) => {
    if (!renderFacts) return readRenderFact(kind, target, args);
    let facts = renderFacts.get(target);
    if (!facts) renderFacts.set(target, facts = new Map());
    const key = `${kind}:${typeof args === 'object' ? JSON.stringify(args) : args}`;
    if (!facts.has(key)) {
      const value = readRenderFact(kind, target, args);
      facts.set(key, value);
      renderInputs.push([kind, target, args, renderValue(kind, value)]);
    }
    return facts.get(key);
  };
  const styleProperty = (target, property) => renderFact('style', target, property);
  const visibilityOf = (target, options) => renderFact('visible', target, options);
  const boxOf = target => renderFact('box', target);
  const textBoxOf = target => renderFact('textBox', target);
  const nativeProperty = (target, property) => renderFact('prop', target, property);
  // Password and agent-typed secret values never leave the page: only their length is observed.
  const secret = e => nativeProperty(e, 'type') === 'password' || nativeProperty(nativeProperty(e, 'dataset'), 'fastbrowseSecret') === '1';
  // The text selected in `doc`, empty when none is. A text field keeps its selection to itself, out of the document's,
  // so the focused field's is read from it, and a focused frame's editor keeps it in the frame's document. A selection
  // made with Ctrl+A or a drag edits nothing, yet it is what the next key or toolbar press acts on: left out, it read
  // as "nothing visible changed" and counted toward the stall rule (#1574). Selected secret text never leaves the page:
  // it is told as one dot a character, as a password field shows it, so selecting it still reads as a change.
  const selectionOf = doc => {
    let field = doc.activeElement;
    while (field?.shadowRoot?.activeElement) field = field.shadowRoot.activeElement;
    if (field?.tagName === 'IFRAME' || field?.tagName === 'FRAME') {
      const inner = childDocument(field);
      return inner && granted(inner) ? selectionOf(inner) : '';
    }
    const hidden = length => '\u2022'.repeat(length);
    if (field?.tagName === 'INPUT' || field?.tagName === 'TEXTAREA') {
      const start = nativeProperty(field, 'selectionStart'), end = nativeProperty(field, 'selectionEnd');
      if (typeof start !== 'number' || !(end > start)) return '';
      return secret(field) ? hidden(end - start) : nativeProperty(field, 'value').slice(start, end);
    }
    const selected = doc.getSelection();
    const text = selected?.toString() ?? '';
    if (!text) return '';
    const touchesSecret = (field && secret(field))
      || [...doc.querySelectorAll('[data-fastbrowse-secret="1"]')].some(e => selected.containsNode(e, true));
    return touchesSecret ? hidden(text.length) : text;
  };
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
    // React's scheduler runs the update a click starts, and the client-side navigation a router begins with it, in
    // a task it posts through a MessageChannel rather than a timer. A settle that held only timers and frames answered
    // before such a navigation began, so the run read the page it was leaving beside a screenshot of the next one,
    // and Jev took the mismatch for a site failure (#1336). A message posted in an action's turn is that action's work
    // until the port it was posted to delivers it, and the delivery is a turn of the same action. A port sent to
    // another context delivers there, never here, so a transferred port's messages stop holding a settle.
    const NativeChannel = window.MessageChannel;
    if (NativeChannel) {
      const peers = new WeakMap();
      // Each port's undelivered messages in the order they were posted: the work of the action each was posted in, or
      // null for one posted outside any action's turn. Every post is listed because every delivery takes the oldest:
      // a list of action posts alone handed a background message's delivery the click's entry, and the settle answered
      // before the click's render.
      const inboxes = new WeakMap();
      const deliver = event => {
        const work = inboxes.get(event.currentTarget)?.shift();
        if (!work) return;
        registry.pendingWork.delete(work);
        registry.inputTurn(work.action);
      };
      // A port that closes or leaves for another context delivers nothing more here, so what waits at it is no one's
      // work, and later posts to it are not tracked.
      const forget = port => {
        for (const work of inboxes.get(port) ?? []) if (work) registry.pendingWork.delete(work);
        inboxes.delete(port);
        peers.delete(port);
      };
      const listen = EventTarget.prototype.addEventListener;
      window.MessageChannel = class MessageChannel extends NativeChannel {
        constructor() {
          super();
          peers.set(this.port1, this.port2);
          peers.set(this.port2, this.port1);
          for (const port of [this.port1, this.port2]) {
            inboxes.set(port, []);
            // Registered before the page can set its own handler, so the delivery's turn is open when it runs.
            listen.call(port, 'message', deliver);
          }
        }
      };
      // The transfer list in `args[at]`, read once and handed to Chrome as read: an options getter or an iterable can
      // change the page or answer differently on a second read, so the ports tracked must be the ports sent. `list`
      // says the argument may be the list itself, which Chrome takes when it is iterable; `keys` names the options
      // members Chrome reads, in its order, or is empty where only a list is taken.
      const transferred = (args, at, list, keys) => {
        const given = args[at];
        if (given === null || typeof given !== 'object') return undefined;
        const iterable = value =>
          value !== null && typeof value === 'object' && typeof value[Symbol.iterator] === 'function';
        if (list && iterable(given)) return (args[at] = [...given]);
        if (!keys.length) return undefined;
        const options = {};
        for (const key of keys) {
          const value = given[key];
          if (value !== undefined) options[key] = key === 'transfer' && iterable(value) ? [...value] : value;
        }
        args[at] = options;
        return options.transfer;
      };
      // Run only once Chrome has sent the message: a send that throws, such as a DataCloneError, leaves the ports here.
      const away = list => {
        if (Array.isArray(list)) for (const port of list) if (inboxes.has(port)) forget(port);
      };
      const nativeClose = MessagePort.prototype.close;
      MessagePort.prototype.close = function () {
        const peer = peers.get(this);
        forget(this);
        // Closing a sender stops new posts, but its peer can still deliver messages already queued there.
        if (peer) peers.delete(peer);
        return Reflect.apply(nativeClose, this, arguments);
      };
      const nativePost = MessagePort.prototype.postMessage;
      MessagePort.prototype.postMessage = function () {
        const args = [...arguments];
        const list = transferred(args, 1, true, ['transfer']);
        const result = Reflect.apply(nativePost, this, args);
        away(list);
        const inbox = inboxes.get(peers.get(this));
        if (inbox) {
          const work =
            registry.turn === null ? null : { action: registry.turn, due: performance.now(), nextFrame: false };
          if (work) registry.pendingWork.add(work);
          inbox.push(work);
        }
        return result;
      };
      for (const target of [window, window.Worker?.prototype, window.ServiceWorker?.prototype]) {
        const post = target?.postMessage;
        if (typeof post !== 'function') continue;
        target.postMessage = function () {
          const args = [...arguments];
          // window.postMessage(message, targetOrigin, transfer) takes a list third, and (message, options) options
          // second; a worker takes a list or options second.
          const list =
            target !== window
              ? transferred(args, 1, true, ['transfer'])
              : args.length > 2
                ? transferred(args, 2, true, [])
                : transferred(args, 1, false, ['targetOrigin', 'transfer']);
          const result = Reflect.apply(post, this, args);
          away(list);
          return result;
        };
      }
    }
  }
  // What was offered to be acted on; `ids` also names hover targets and fingerprinted nodes.
  registry.controls ||= new WeakSet();
  // A change counter, so a read can tell later whether the page it described has moved since. Everything here can
  // change what a read would report without rewriting a node: pointer entry reveals a `:hover` rule, a font or image
  // that finishes loading shifts the layout under it, a document observer sees neither a shadow root nor a child
  // document, and a sheet inserted through the CSSOM changes what is shown with no record to observe.
  // The page keeps one registry across fastbrowse versions, and an earlier one may have set it up without watching
  // other roots: each version that changes what is tracked sets it up again, keeping the counters already handed out.
  if (registry.revTracking !== 2) {
    registry.revTracking = 2;
    registry.rev ??= 0;
    // A kept read stands only while the counter has not moved, so a change also lets go of it: otherwise it would hold
    // the roots, frames and elements it walked, a removed child document among them, until the next read.
    const bumped = () => {
      registry.rev++;
      registry.kept = null;
    };
    const events = [
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
    ];
    // A document observer sees neither a shadow root nor a child document, so each root a read walks into is watched
    // on its own. A shadow root hears the events its tree dispatches, including those that do not cross into the
    // document (`change`, `scroll`, `load`); a child document's window hears its own input, scrolling and resizing.
    const observed = new WeakSet();
    registry.observeRoot = root => {
      if (observed.has(root)) return;
      observed.add(root);
      new MutationObserver(records => {
        records = pageRecords(records);
        if (!records.length) return;
        bumped();
        // A same-origin iframe moves the page as the top document does: the settle and the changed text below watch the
        // top document only, so its frames report here.
        if (root !== document) registry.moved?.(records);
      }).observe(root, { subtree: true, characterData: true, childList: true, attributes: true, attributeOldValue: true });
      const target = root.nodeType === Node.DOCUMENT_NODE ? root.defaultView : root;
      if (root.nodeType === Node.DOCUMENT_NODE && root !== document) registry.listenInput?.(target);
      // Passive, as Chrome makes a wheel listener only on a window, document or body: one on a shadow root would hold
      // every scroll inside it to the main thread.
      for (const type of events) target.addEventListener(type, bumped, { capture: true, passive: true });
    };
    registry.observeRoot(document);
    // A short digest of a long text, so a fingerprint is kept and compared without holding the text itself.
    const hash = text => {
      let h = 0x811c9dc5;
      for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 16777619);
      return `${text.length}:${h >>> 0}`;
    };
    // The CSSOM edits a rule in place (`rule.style.display = 'none'`) with no record to observe and no rule added, so
    // every readable rule is fingerprinted by its text. A sheet another origin serves is not readable here, and its
    // rules are not what changed ours.
    registry.sheets = (doc = document) => {
      const parts = [];
      for (const sheet of [...doc.styleSheets, ...(doc.adoptedStyleSheets || [])]) {
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
    registry.live = (root = document) => {
      const parts = [];
      for (const e of root.querySelectorAll('input,textarea,select,button')) {
        const chosen = e.type === 'select-multiple' ? [...e.selectedOptions].map(o => o.index).join() : e.selectedIndex;
        parts.push(JSON.stringify([e.value, e.checked, e.indeterminate, chosen, e.validationMessage]));
      }
      return hash(parts.join('\n'));
    };
    // Each kept read is numbered, so a caller answered "unchanged" can tell whether the read it holds is that one.
    registry.reads ??= 0;
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
    'menuitemcheckbox',
    'treeitem',
    'listbox',
    'tablist',
    'grid',
    'treegrid',
    'tree',
    'menu',
    'menubar',
    'option',
    'gridcell',
    'combobox',
    'textbox',
    'searchbox',
    'spinbutton',
    'slider',
  ];
  // jQuery UI's datepicker prev/next are anchors with no href, appended to <body> once the field is focused:
  // they carry no destination, only a click handler, so they need naming here to be walked at all.
  const SELECTOR =
    'a[href],a[data-handler][data-event="click"],a[onclick],a[role="button"],' +
    'button,input,textarea,select,summary,label,[contenteditable="true"],[draggable="true"],[ondrop],' +
    ARIA_ROLES.map(role =>
      `[role="${role}"]` + (['listbox', 'tablist', 'grid', 'treegrid', 'tree', 'menu', 'menubar'].includes(role) ? '[tabindex]' : ''),
    ).join(',');
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
  // What a script listens for to show something while the pointer is over an element.
  const HOVER = ['mouseenter', 'mouseover', 'pointerenter', 'pointerover'];
  let listenerStates = new Map();
  let hoverStates = new Map();
  // The function answers only for elements of the realm it was read in, and a same-origin frame's document has a realm
  // of its own: read from the parent, a frame's listened span was no control (#1312). So a frame's elements are asked of
  // that document's own function, through its `reader` mode, which throws `own.unread` there until it has read its own.
  const readers = new Map([[document, (e, types) => hears(own.listeners, e, types)]]);
  const readerOf = doc => {
    if (!readers.has(doc)) {
      const reader = doc.defaultView?.__fastbrowseScripts?.[own.key]?.('reader');
      if (typeof reader !== 'function') throw new Error(own.unread);
      readers.set(doc, reader);
    }
    return readers.get(doc);
  };
  // The root element and the body hear clicks on the whole page, as the document does, so neither counts.
  const listened = (e, types = POINTER, states = listenerStates) => {
    if (e === e.ownerDocument.documentElement || e === e.ownerDocument.body) return false;
    if (states?.has(e)) return states.get(e);
    const value = readerOf(e.ownerDocument)(e, types);
    states?.set(e, value);
    return value;
  };
  // React 17+ attaches no listener to an element: its root hears every hover and calls the handler the element's
  // current props name, which it keeps on a `__reactProps$<id>` expando (`__reactEventHandlers$<id>` before 17). Chrome's
  // reader sees only the root, so the props are the one place the page says which element reveals something on hover.
  // The expando is React's own record of the handlers it will call, not a guess at the page's markup.
  const REACT_HOVER = [
    'onMouseEnter', 'onMouseOver', 'onPointerEnter', 'onPointerOver',
    'onMouseEnterCapture', 'onMouseOverCapture', 'onPointerEnterCapture', 'onPointerOverCapture',
  ];
  const reactHover = e => {
    const key = Object.keys(e).find(k => k.startsWith('__reactProps$') || k.startsWith('__reactEventHandlers$'));
    const props = key && e[key];
    return Boolean(props) && REACT_HOVER.some(name => typeof props[name] === 'function');
  };
  const hoverListened = e => {
    if (hoverStates?.has(e)) return hoverStates.get(e);
    const value = listened(e, HOVER, hoverStates) || reactHover(e);
    hoverStates?.set(e, value);
    return value;
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
    // A frame's window hears its own events, so each same-origin child document asks for the same listeners.
    registry.listenInput = target => {
      for (const type of [
        'pointerdown', 'pointerup', 'mousedown', 'mouseup', 'click', 'auxclick', 'dblclick',
        'keydown', 'keyup', 'keypress', 'submit', 'input', 'change', 'wheel', 'scroll',
      ]) {
        target.addEventListener(type, input, true);
      }
    };
    registry.listenInput(window);
    registry.moved = records => {
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
    };
    new MutationObserver(records => {
      records = pageRecords(records);
      if (records.length) registry.moved(records);
    }).observe(document, { subtree: true, characterData: true, childList: true, attributes: true, attributeOldValue: true });
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
  // What a Wait compares, as one function so an observation can hold the fingerprint of the page it described.
  const takeFingerprint = () => {
    let hash = 2166136261;
    const hashText = text => {
      for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 16777619);
    };
    // Players' clocks stay out of the hash: one that plays moves by itself, which must not make a control stale.
    // They are kept beside it, rounded as readMedia rounds them, for a Wait to tell a moving clock.
    const clocks = [];
    // What a Wait compares beside the hash, from the same walk so it covers the same documents: each player's state as
    // readMedia reports it, and the documents walked, whose events a Wait listens to.
    const players = [];
    const documents = [];
    const include = root => {
      if (root.nodeType === Node.DOCUMENT_NODE) documents.push(root);
      hashText(root.body?.innerText ?? root.textContent ?? '');
      const selected = root.nodeType === Node.DOCUMENT_NODE ? selectionOf(root) : '';
      if (selected) hashText(JSON.stringify(['selection', selected]));
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
          ('checked' in e ||
            'selected' in e ||
            e.hasAttribute('aria-checked') ||
            e.hasAttribute('aria-pressed') ||
            e.hasAttribute('aria-selected')) &&
          visibilityOf(e, { checkOpacity: !nativeChoice(e), checkVisibilityCSS: true }) &&
          (!nativeChoice(e) || shownThrough(e))
        )
          hashText(
            JSON.stringify([
              nativeProperty(e, 'checked') ?? null,
              e.selected ?? null,
              e.getAttribute('aria-checked'),
              e.getAttribute('aria-pressed'),
              e.getAttribute('aria-selected'),
            ]),
          );
        // A fill whose only visible effect is enabling Save changed the page: a Wait that ignored it waited out its
        // whole budget for a change that had already happened (#1415). Only the effective state counts: an absent
        // aria-disabled and "false" both mean enabled, and a page that adds the "false" as it hydrates changed nothing.
        if (
          (e.matches('button,input,select,textarea,fieldset') || e.getAttribute('aria-disabled') === 'true') &&
          e.checkVisibility({checkVisibilityCSS: true})
        )
          hashText(JSON.stringify([e.matches(':disabled'), e.getAttribute('aria-disabled') === 'true']));
        // A role key can change only the widget's announced value, without editing text or an input field.
        if (
          (e.matches('input[type="range"],input[type="number"]') || ['slider', 'spinbutton'].includes(e.getAttribute('role'))) &&
          e.checkVisibility({checkVisibilityCSS: true})
        ) hashText(JSON.stringify([e.matches('input[type="range"],input[type="number"]') ? e.value : null, e.getAttribute('aria-valuenow'), e.getAttribute('aria-valuetext')]));
        // readMedia lists the players of documents only, not of shadow roots.
        if (root.nodeType === Node.DOCUMENT_NODE && (e.tagName === 'AUDIO' || e.tagName === 'VIDEO')) {
          clocks.push(Math.round(e.currentTime * 10) / 10);
          players.push([
            e.paused,
            e.ended,
            e.muted,
            Number.isFinite(e.duration) ? Math.round(e.duration * 10) / 10 : null,
          ]);
        }
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
      players: JSON.stringify(players),
      documents,
      // Whether this document may be read at all, which a Wait asks of a frame in a process of its own.
      granted: granted(document),
    };
  };
  if (mode === 'fingerprint') return takeFingerprint();
  if (!document.body) return null;
  // A child document's address when it may be read, or null.
  const readableFrame = inner => (inner?.body && granted(inner) ? inner.URL : null);
  // A page that has not moved since the read it last gave is not read again: the caller still holds that read, and
  // deriving it and sending it back is what an observation costs on a dense page. Anything that can change what a
  // read would say without rewriting a node is asked first, so a reuse is only a page that is provably still.
  const kept = registry.kept?.[mode];
  const rendered = own.renderReads?.[mode];
  // What a tree says without rewriting a node: a script sets a field's value with no event, the CSSOM edits a rule in
  // place, a child document's own history moves its address, scrolls or resizes. A transition or animation in flight
  // moves what is on screen between one look and the next, so a read taken while one runs does not stand.
  const rootState = root => {
    if (root.nodeType !== Node.DOCUMENT_NODE) return [registry.live(root), root.getAnimations().length];
    const view = root.defaultView;
    return [registry.live(root), registry.sheets(root), root.URL, root.title, root.readyState, root.hidden,
      view.scrollX, view.scrollY, view.innerWidth, view.innerHeight, root.getAnimations().length];
  };
  const still =
    reuse &&
    kept &&
    kept.mode === mode &&
    rendered?.read === kept.read && rendered?.document_key === kept.document_key &&
    (!delta || (base?.document_key === kept.document_key && base?.read === kept.read)) &&
    kept.rev === registry.rev &&
    // A frame shows another document, or one it could not read becomes readable, with no mutation in its parent.
    kept.frames?.every(([frame, inner, url]) => childDocument(frame) === inner && readableFrame(inner) === url) &&
    // attachShadow emits no mutation, so every element of every tree read must still hold only the roots walked.
    kept.roots.every(([, elements]) => elements.every(e => !e.shadowRoot || kept.walked.has(e.shadowRoot))) &&
    kept.roots.every(([root], i) => sameRenderValue(kept.states[i], rootState(root)) && kept.states[i].at(-1) === 0) &&
    kept.listenerAPI === Boolean(own.listeners) &&
    // Upgrading a form-associated custom element can change HTML label ownership without a DOM mutation.
    !kept.undefinedElements?.some(e => e.isConnected && e.matches(':defined')) &&
    // Adding, removing or aborting a native listener writes no DOM node or attribute.
    kept.listenerStates?.every(([e, value]) => listened(e) === value) &&
    kept.hoverStates?.every(([e, value]) => hoverListened(e) === value) &&
    kept.inputs.every(([kind, target, args, value]) =>
      sameRenderFact(kind, value, readRenderFact(kind, target, args)));
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
    listenerStates = null;
    hoverStates = null;
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
    own.elementChildren = new WeakMap();
    own.elementOrders = new WeakMap();
    own.controlMatches = new WeakMap();
  }
  own.queryResults ||= new WeakMap();
  own.names ||= new WeakMap();
  own.elementChildren ||= new WeakMap();
  own.elementOrders ||= new WeakMap();
  own.controlMatches ||= new WeakMap();
  const flushedTrees = new Set();
  const invalidateQueries = (records, root) => {
    const bumped = new Set();
    for (const record of pageRecords(records)) {
      for (let node = record.target; node && !bumped.has(node); node = node.parentNode) {
        bumped.add(node);
        own.queryResults.delete(node);
        own.domVersions.set(node, (own.domVersions.get(node) || 0) + 1);
      }
      if (record.type === 'childList') own.elementChildren.delete(record.target);
      for (const added of record.addedNodes || []) {
        if (added.nodeType !== 1) continue;
        // A removed subtree can be edited while no observer watches it, then inserted somewhere else.
        for (const node of [added, ...added.querySelectorAll('*')]) {
          own.elementChildren.delete(node);
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
  // Element order changes only when an element enters or leaves a child list. Saved edges let a changed
  // subtree renew the order without asking the browser to walk its unchanged siblings again.
  const indexedElements = scope => {
    const root = flushTree(scope);
    const version = own.structureVersions.get(root) || 0;
    const saved = own.elementOrders.get(scope);
    if (saved?.root === root && saved.version === version) return saved.elements;
    const childrenOf = parent => {
      const entry = own.elementChildren.get(parent);
      if (entry?.root === root) return entry.children;
      const children = [];
      for (let node = parent.firstElementChild; node; node = node.nextElementSibling) children.push(node);
      if (children.length) own.elementChildren.set(parent, { root, children });
      return children;
    };
    const elements = [];
    const pending = [{ children: childrenOf(scope), next: 0 }];
    while (pending.length) {
      const level = pending[pending.length - 1];
      if (level.next === level.children.length) {
        pending.pop();
        continue;
      }
      const node = level.children[level.next++];
      elements.push(node);
      const children = childrenOf(node);
      if (children.length) pending.push({ children, next: 0 });
    }
    own.elementOrders.set(scope, { root, version, elements });
    return elements;
  };
  const listedControl = e => {
    const root = e.getRootNode();
    const version = own.domVersions.get(e) || 0;
    let entry = own.controlMatches.get(e);
    if (entry?.root !== root || entry.version !== version) {
      entry = { root, version, value: e.matches(SELECTOR) };
      own.controlMatches.set(e, entry);
    }
    return entry.value;
  };
  flushTree(document);
  const innerTextOf = e => renderFact('text', e);

  const safe = e => nativeProperty(e, 'type') !== 'hidden';
  const reveal = e => (typeof nativeProperty(e, 'value') !== 'string' ? null : secret(e) ? '•'.repeat(nativeProperty(e, 'value').length) : nativeProperty(e, 'value'));
  // What a select has chosen. selectedIndex names only the first chosen option, so a list that allows several gives
  // every chosen index: a later option chosen or taken out changes it.
  const chosen = e =>
    nativeProperty(e, 'multiple') && e.tagName === 'SELECT' ? [...renderFact('selected', e)].map(o => o.index) : (nativeProperty(e, 'selectedIndex') ?? null);
  // Tag pickers keep the field they replace on the page, clipped to nothing, and checkVisibility() ignores clip: a
  // control whose own clip or clip-path leaves no area is not there for a person. A hidden checkbox, radio or file
  // input is still reached through its label. Text is still read, as a page states facts in visually hidden text.
  const clippedAway = e => {
    const rect = /^rect\((.+)\)$/.exec(styleProperty(e, 'clip'));
    const inset = /^inset\(([^)]*?)(?:\s+round\s.*)?\)$/.exec(styleProperty(e, 'clipPath'));
    // Almost every element has neither, and its box is not read.
    if (!rect && !inset) return false;
    const { width, height } = boxOf(e);
    if (rect && ['absolute', 'fixed'].includes(styleProperty(e, 'position'))) {
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
    visibilityOf(e, { checkVisibilityCSS: true }) &&
    styleProperty(e, 'pointerEvents') !== 'none' &&
    !onChain(
      e,
      a =>
        styleProperty(a, 'opacity') === '0' &&
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
      styleProperty(e, 'pointerEvents') !== 'none' &&
      ![...(e.labels || [])].some(l => visibilityOf(l, { checkOpacity: true, checkVisibilityCSS: true }));
    if (
      e.closest('[aria-hidden="true"],[inert]') ||
      (!visibilityOf(e, { checkOpacity: !choice, checkVisibilityCSS: true }) && (choice || !fadingIn(e))) ||
      leaving(e) ||
      (choice && !shownThrough(e)) ||
      (clippedAway(e) && (e.matches(SELECTOR) || pressable(e) || focusableText(e) || draggedByPointer(e)))
    )
      return false;
    if (!choice) return true;
    const r = boxOf(e);
    return r.width > 0 && r.height > 0;
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
      : e.tagName === 'LABEL' && ['checkbox', 'radio', 'file'].includes(nativeProperty(e.control, 'type'))
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
      if (!visibilityOf(icon, { checkOpacity: true, checkVisibilityCSS: true })) continue;
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
  const shownNow = memo(e => e.localName === 'title' || visibilityOf(e, { checkOpacity: true, checkVisibilityCSS: true }));
  const shown = e => {
    const value = shownNow(e);
    nameReads?.shown.set(e, value);
    return value;
  };
  const nameValue = e => {
    const value = nativeProperty(e, 'value');
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
  // An image's alt text and the words beside it often say the same thing: a card holds a picture whose alt is its title
  // and a span showing that title. Accname would name the card with both, so every label read "Test image Test image"
  // (#1457). The alt text only stands in for the picture, so it is left out when a sibling already shows it.
  const namedByContent = (e, seen) => {
    const flat = text => text.replace(/\s+/g, ' ').trim();
    // A picture may sit in a wrapper (<picture>, a thumbnail span): the wrapper counts as the picture when it holds
    // an image and no words of its own. A <source> beside the <img> in a <picture> shows nothing itself.
    const isPicture = n =>
      n.nodeType === 1 &&
      (n.tagName === 'IMG' ||
        (n.children.length > 0 &&
          [...n.childNodes].every(
            c => (c.nodeType === 3 ? !c.textContent.trim() : c.nodeType !== 1 || c.tagName === 'SOURCE' || isPicture(c)),
          )));
    // A wrapper that is named only by its content ("<span><span>Test image</span><span>New</span></span>") is read
    // through, so each picture and each run of words keeps what it is: flattening the wrapper to one string lost
    // which part was the picture's alt and left the title doubled beside a badge.
    const namedByOwnContent = n =>
      !n.getAttribute('aria-labelledby') &&
      !n.getAttribute('aria-label') &&
      !n.hasAttribute('alt') &&
      !labelsFor(n).length &&
      !['button', 'submit', 'reset'].includes(nativeProperty(n, 'type')) &&
      !['INPUT', 'SELECT', 'TEXTAREA'].includes(n.tagName) &&
      !FIELD_CONTAINERS.has(n.getAttribute('role'));
    const contentParts = parent =>
      [...parent.childNodes].flatMap(n => {
        if (n.nodeType === 3) return [{ image: false, text: n.textContent }];
        if (n.nodeType !== 1 || CODE.has(n.tagName) || n.getAttribute('aria-hidden') === 'true') return [];
        if (!shown(n) && shown(parent)) return [];
        // A wrapper of several images is read through, so each image stays its own part and drops its own repeated title.
        if (isPicture(n) && (n.tagName === 'IMG' || n.querySelectorAll('img').length <= 1))
          return [{ image: true, text: labelFrom(n, seen) }];
        if (!seen.has(n) && namedByOwnContent(n)) {
          seen.add(n);
          nameReads?.nodes.add(n);
          const inner = contentParts(n);
          return inner.some(part => part.text.trim())
            ? inner
            : [{ image: false, text: n.getAttribute('title') || '' }];
        }
        return [{ image: false, text: labelFrom(n, seen) }];
      });
    const parts = contentParts(e);
    const words = parts.filter(part => !part.image).map(part => part.text);
    // Words split across inline nodes ("Test <b>image</b>") repeat the alt as a whole.
    // A badge or status beside the title must not hide it, so any run of adjacent texts may match. A run is grown only
    // while it is still the start of the alt: building every run made strings quadratic in the node count, and a
    // control named by 1,000 spans exhausted the renderer's heap.
    const shownBeside = alt => {
      const target = flat(alt);
      if (!target) return false;
      for (let i = 0; i < words.length; i++) {
        let run = '';
        for (let j = i; j < words.length; j++) {
          run = flat(`${run} ${words[j]}`);
          if (run === target) return true;
          if (!target.startsWith(run)) break;
        }
      }
      return false;
    };
    return parts
      .filter(part => !part.image || !shownBeside(part.text))
      .map(part => part.text)
      .join(' ')
      .trim();
  };
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
      (['button', 'submit', 'reset'].includes(nativeProperty(e, 'type')) ? nameValue(e) : '') ||
      e.getAttribute('alt') ||
      // A field's text is its value, not its name: a list-style select shows every option, and a combobox widget its
      // chosen chips, remove buttons and open options. Accname never names these from content either. A textarea's
      // child text is the value it was served with: a settings page's 8 KB description became the field's label, and
      // the gate read that text as what a click on the field would do (#885).
      (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.tagName) || FIELD_CONTAINERS.has(e.getAttribute('role'))
        ? ''
        : namedByContent(e, seen)) ||
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
        kept.values.every(([node, value]) => nativeProperty(node, 'value') === value)) return kept.text;
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
    // A canvas holds no elements to take a click, so a click on it is heard wherever the page listens for it: a map
    // library listens on the viewport that also holds its zoom buttons, which as a container of controls is no control
    // itself, and left the drawn map with nothing to press.
    if (e.tagName === 'CANVAS' && !e.parentElement?.closest(SELECTOR)) {
      for (let a = e; a && a !== e.ownerDocument.body; a = a.parentElement)
        if (a.hasAttribute('onclick') || typeof nativeProperty(a, 'onclick') === 'function' || listened(a)) return true;
    }
    const declared =
      e.hasAttribute('onclick') ||
      typeof nativeProperty(e, 'onclick') === 'function' ||
      (styleProperty(e, 'cursor') === 'pointer' &&
        (!e.parentElement || styleProperty(e.parentElement, 'cursor') !== 'pointer'));
    if (!(declared || listened(e)) || e.parentElement?.closest(SELECTOR) || [...descendants(e, SELECTOR)].some(visible))
      return false;
    // A listener can sit on a container to hear its children's clicks, as React's does on its root, so one holding a
    // pressable element leaves the click to it.
    return declared || ![...descendants(e, '*')].some(c => (pressable(c) || draggedByPointer(c)) && visible(c));
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

  // A sortable list driven by the page's own pointer script (a grip handle, no `draggable`, no `ondrop`, React's
  // listeners on the root) states nothing in markup, but its stylesheet gives the element the `grab` or `move` cursor:
  // the page's own statement that the pointer drags it, read like a `:hover` rule. The cursor inherits, so only the
  // element it starts on counts, as with `pressable`.
  const DRAG_CURSORS = ['grab', 'move'];
  const draggedByPointer = memo(e =>
    DRAG_CURSORS.includes(styleProperty(e, 'cursor')) &&
    (!e.parentElement || styleProperty(e.parentElement, 'cursor') !== styleProperty(e, 'cursor')));

  const roleOf = e => {
    if (sourceOf(e) !== e) return nativeProperty(sourceOf(e), 'type') === 'file' ? 'textbox' : nativeProperty(sourceOf(e), 'type');
    const explicit = e.getAttribute('role');
    if (ARIA_ROLES.includes(explicit)) return explicit;
    if (e.tagName === 'BUTTON' || e.tagName === 'SUMMARY') return 'button';
    // An anchor with nowhere to go acts through its click handler alone, like the jQuery UI datepicker's
    // Next/Prev: a destination is what makes it a link, so without one it is offered as a button instead.
    if (e.tagName === 'A') return e.hasAttribute('href') ? 'link' : 'button';
    if (e.tagName === 'SELECT') return 'combobox';
    if (e.tagName === 'TEXTAREA' || nativeProperty(e, 'isContentEditable')) return 'textbox';
    if (e.tagName === 'INPUT') {
      if (nativeProperty(e, 'type') === 'file') return 'textbox';
      if (['checkbox', 'radio'].includes(nativeProperty(e, 'type'))) return nativeProperty(e, 'type');
      if (['button', 'submit', 'reset', 'image'].includes(nativeProperty(e, 'type'))) return 'button';
      if (nativeProperty(e, 'type') === 'search') return 'searchbox';
      if (nativeProperty(e, 'type') === 'number') return 'spinbutton';
      // HTML-AAM maps a range input to the slider role, whose keys step it.
      if (nativeProperty(e, 'type') === 'range') return 'slider';
      if (['text', 'email', 'url', 'tel', 'password', ...DATE_TYPES].includes(nativeProperty(e, 'type'))) return 'textbox';
    }
    // A bare draggable element or a declared drop target has no ARIA role of its own, yet it is a real
    // element to act on. Name it generic rather than null so the control walk does not drop it before the
    // drag it offers can be seen.
    if (e.getAttribute('draggable') === 'true' || e.hasAttribute('ondrop') || draggedByPointer(e)) return 'generic';
    // A focusable label is pressed through the page's own listener, as a button is.
    if (focusableText(e)) return 'button';
    return pressable(e) ? 'button' : null;
  };

  // What submitting from a control sends, and whether its submit control is disabled now. A disabled submit sends
  // nothing, but it is the same submit once enabled: the pairing key reads the semantics apart from that state.
  const submitFacts = memo(e => {
    const form = e.form;
    const explicit = ['BUTTON', 'INPUT'].includes(e.tagName) && ['submit', 'image'].includes(nativeProperty(e, 'type'));
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
    if (!form || (!explicit && (e.tagName !== 'INPUT' || !implicit.includes(nativeProperty(e, 'type'))))) return null;
    const submit = explicit ? e : [...form.getRootNode().querySelectorAll('button,input')].find(
      c => c.form === form && (nativeProperty(c, 'type') === 'submit' || nativeProperty(c, 'type') === 'image'),
    );
    const disabled = !!submit?.matches(':disabled');
    if (!submit && [...form.elements].filter(c => c.tagName === 'INPUT' && implicit.includes(nativeProperty(c, 'type'))).length > 1)
      return null;
    // A page can make any of these properties return something other than text. Coursera's React app left an
    // HTMLInputElement where a method string belongs and `JSON.stringify` threw on React's circular fiber, which ended
    // the run as a browser error; a value that is not text is no more use to a caller than none.
    const shown = v => (typeof v === 'string' || typeof v === 'number' ? String(v) : '');
    const semantics = JSON.stringify({
      label: shown(submit ? labelOf(submit) : labelOf(form)),
      method: shown(submit?.getAttribute('formmethod') || form.method),
      action: shown(submit?.getAttribute('formaction') || form.action),
      text: shown(innerTextOf(form)),
    });
    return { disabled, semantics };
  });
  const submitSemantics = e => {
    const facts = submitFacts(e);
    return facts && !facts.disabled ? facts.semantics : null;
  };

  const framePath = doc => {
    if (doc === document) return null;
    const owner = doc.defaultView.frameElement;
    return `${framePath(owner.ownerDocument) || 'root'}/${identity(owner)}`;
  };

  // Traverse light DOM plus any open shadow roots, and same-origin same-process nested iframes.
  let inaccessible = 0;
  // Every tree the walk read with its elements, and every frame with the document it showed and that document's
  // address when it was read, so a later read can prove each still holds before answering unchanged.
  const walkedRoots = [];
  const walkedFrames = [];
  function* walk(root) {
    registry.observeRoot(root);
    const elements = indexedElements(root);
    walkedRoots.push([root, elements]);
    for (const e of elements) {
      if (listedControl(e) || pressable(e) || focusableText(e) || draggedByPointer(e)) yield e;
      if (e.shadowRoot) yield* walk(e.shadowRoot);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        const inner = childDocument(e);
        const url = readableFrame(inner);
        walkedFrames.push([e, inner, url]);
        if (url !== null) yield* walk(inner);
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
  const scopeDigest = memo(scope => {
    let text = innerTextOf(scope) || '';
    for (const e of registry.selfUpdating) if (scope.contains(e)) text = text.replace(innerTextOf(e) || '', '');
    return digest(text.slice(0, GUARD_TEXT_CHARS));
  });
  // `stable` blanks the control's own current state, for `retargetKey` below.
  registry.guard = (e, id, stable = false) => {
    if (!e?.isConnected || !visible(e)) return null;
    const axis = registry.scrollAxes?.get(id);
    if (axis) return [identity(e), 'scroll', axis,
      e.getAttribute('aria-label'), e.getAttribute('aria-labelledby'), e.getAttribute('title'),
      e.ownerDocument.location.origin, e.ownerDocument.defaultView.performance.timeOrigin, identity(e)];
    const source = sourceOf(e);
    // A redraw that disables or enables a label's field is still the same control, so the pairing key skips the
    // field's disabled state; the live guard still refuses it.
    const sourceDisabled = () => source.matches(':disabled') || !!source.closest('[aria-disabled="true"]');
    if (source !== e && (source.closest('[inert]') || (!stable && sourceDisabled()))) return null;
    const scope = e.closest('form,dialog,[role="dialog"],article,li,tr,[role="row"]') || e.parentElement;
    const own = value => (stable ? null : value);
    // An absent aria-disabled and "false" both mean enabled. Google Flights adds the "false" as it hydrates, and
    // the raw attribute turned the run's first click stale.
    return [
      identity(e),
      roleOf(e),
      labelOf(e) || fallbackLabel(e),
      own(reveal(source)),
      own(nativeProperty(source, 'checked') ?? null),
      own(chosen(e)),
      nativeProperty(e, 'readOnly') ?? null,
      // Disabling or enabling is the control's own state: a redraw that also enabled Save left it unpaired, so the
      // change was reported as one Save removed and another shown. A container's aria-disabled disables it as its
      // own does, as the capture reads it.
      own(e.matches(':disabled')),
      own(!!e.closest('[aria-disabled="true"]')),
      own(e.getAttribute('aria-expanded')),
      own(e.getAttribute('aria-checked')),
      own(e.getAttribute('aria-pressed')),
      own(e.getAttribute('aria-selected')),
      e.getAttribute('href'),
      scope ? scopeDigest(scope) : '',
      e.ownerDocument.location.origin,
      e.ownerDocument.defaultView.performance.timeOrigin,
      // A redraw that replaced and enabled Save changed what it submits from nothing to the form, and left it unpaired.
      stable ? (submitFacts(e)?.semantics ?? null) : submitSemantics(e),
      source.getAttribute('name'),
      source.getAttribute('autocomplete'),
      identity(source),
    ];
  };
  registry.guardKey = (e, id) => {
    const guard = registry.guard(e, id);
    return guard && digest(JSON.stringify(guard));
  };
  // What a control is apart from its node ids and its own current state. A redraw that also fills a field or checks a
  // box changes that state, and keying on it left the replacement unpaired: two rebuilt "Name" fields, one just
  // filled, were reported as one removed and one shown, never as the value set. A retargeted action still compares the
  // whole control, state included, before it follows the key.
  registry.retargetKey = (e, id) => registry.guard(e, id, true)?.slice(1, -1) ?? null;

  const DRAWN_KINDS = { IMG: 'image', CANVAS: 'canvas', VIDEO: 'video' };
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
    if (!safe(e) || !visible(e) || e.closest('[inert]')) continue;
    const source = sourceOf(e);
    if (source !== e && (visible(source) || source.closest('[inert]'))) continue;
    // A disabled control stays in the observation, marked and offered nothing: left out, a Save the page keeps
    // disabled after a fill read the same as a page with no Save, and a run on the CurseForge description editor ended
    // stuck without knowing its edit had not taken (#1415).
    const disabled = [e, source].some(n => n.matches(':disabled') || !!n.closest('[aria-disabled="true"]'));
    const r = boxOf(e),
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
    const root = e.getRootNode();
    const active = renderFact('active', root);
    const activeDescendant = active?.getAttribute('aria-activedescendant');
    // A composite widget can keep DOM focus on its container while its active descendant moves between items.
    const focused = active === e || (!!activeDescendant && root.getElementById?.(activeDescendant) === e);
    const base = {
      id,
      role: rname,
      // A nameless drawn surface is named for what it is: called by its role, an OpenLayers map read as a "button".
      label: named || fallbackLabel(e) || DRAWN_KINDS[e.tagName] || rname,
      offscreen: y < 0 || y >= innerHeight,
      distance: y < 0 || y >= innerHeight ? 1 + Math.abs(y - innerHeight / 2) : 0,
      sensitive: secret(source),
      // The root's own active element, so a field inside a shadow root reads as focused where the document names
      // only its host.
      focused,
      input_type: nativeProperty(source, 'type') || null,
      input_name: source.getAttribute('name') || null,
      max_length: nativeProperty(source, 'maxLength') > 0 ? nativeProperty(source, 'maxLength') : null,
      autocomplete: source.getAttribute('autocomplete') || null,
      frame_origin: e.ownerDocument.location.origin,
      frame_path: framePath(e.ownerDocument),
      form_id: fieldScope(source),
      submit_semantics: submitSemantics(e),
    };
    // An inlined iframe can reload while the document producing this snapshot stays put.
    if (base.form_id !== null && e.ownerDocument !== document) {
      base.frame_document_key = String(e.ownerDocument.defaultView.performance.timeOrigin);
    }
    if (rname === 'link' && nativeProperty(e, 'href')) {
      const u = new URL(nativeProperty(e, 'href'), location.href);
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
        /pagin|pager/i.test(typeof nativeProperty(el, 'className') === 'string' ? nativeProperty(el, 'className') : '') ||
        /pagin|pager/i.test(el.getAttribute('aria-label') || '');
      const container = () => {
        for (let el = e.parentElement; el; el = el.parentElement) if (paging(el)) return true;
        return false;
      };
      if (
        u.origin === location.origin &&
        ((nativeProperty(e, 'rel') && nativeProperty(e, 'rel').split(/\s+/).includes('next')) ||
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
    // A toggle button's on state is aria-pressed, ARIA's checked for a button: an editor's icon-only Bold read as
    // pressed nowhere, so turning it on was "nothing visible changed" and the run went looking for another way.
    if (base.checked === undefined && e.hasAttribute('aria-pressed')) base.checked = e.getAttribute('aria-pressed') === 'true';
    // Input on an image, canvas or video lands on pixels: a streamed remote browser draws its page as a picture
    // that takes clicks, and what it shows has no elements here to read or act on (#1084). A shape inside an SVG is
    // its own element and takes its own click, so it is pictured, not drawn: Leaflet's circle marked drawn had the run
    // asked whether the task needed a click inside it, and stop when it judged so. An image the page itself gives a
    // widget role is that one control: Leaflet's marker, role button, marked drawn drew the step to it as the surface
    // that held the red circle beside it. An element that names itself draggable is a drag source for the page's own
    // drag and drop, dragged onto a drop zone: drawn on, a drag would stay inside it and no zone would be reached. An
    // image the page gives a grab cursor is the same handle for its own pointer script (a sortable card's picture); a
    // canvas with that cursor is a surface panned in place, so it stays drawn.
    const handle = e.getAttribute('draggable') === 'true' || (e.tagName === 'IMG' && draggedByPointer(e));
    if (e.tagName in DRAWN_KINDS && !ARIA_ROLES.includes(e.getAttribute('role')) && !handle) base.drawn = true;
    if (['checkbox', 'radio'].includes(nativeProperty(source, 'type'))) base.checked = nativeProperty(source, 'checked');
    // What the page says the control is and how it is operated, in its own words: a dnd-kit card is a "button" whose
    // `aria-roledescription` is "draggable" and whose described-by text says Space picks it up and the arrows move it.
    // Without both, Jev saw a plain button and clicked it.
    const roleDescription = (e.getAttribute('aria-roledescription') || '').replace(/\s+/g, ' ').trim();
    if (roleDescription) base.role_description = roleDescription;
    const description = describedBy(e);
    if (description && description !== base.label) base.description = description;
    // What the page says about the field. A custom widget marks the element the user sees, not the input
    // underneath it, so both are asked.
    if (nativeProperty(source, 'required') || [e, source].some(n => n.getAttribute('aria-required') === 'true')) base.required = true;
    // Native validation can refuse a form without adding any ARIA attribute or page text. The browser holds that
    // against a field only once it was edited or its form was submitted, as `:user-invalid`: an empty required field a
    // page has just opened or reset is not refused, and told it was, a run that had submitted a wizard filled it again.
    const refused = nativeProperty(source, 'willValidate') === true && renderFact('validity', source) === false && renderFact('state', source, ':user-invalid');
    if ([e, source].some(n => n.getAttribute('aria-invalid') === 'true') || refused) base.invalid = true;
    // Why the browser refused it, in its own words: a refused submit shows that only in a bubble the page text
    // lacks, and a run that saw only `invalid` refilled the field the task named instead of the empty one.
    if (refused && nativeProperty(source, 'validationMessage')) base.validation_message = nativeProperty(source, 'validationMessage');
    if (e.tagName === 'SELECT') {
      base.operations = ['select'];
      base.options = [...e.options].filter(o => !nativeProperty(o, 'disabled') && !o.closest('optgroup[disabled]')).map(o => nativeProperty(o, 'label'));
      base.value = [...renderFact('selected', e)].map(o => nativeProperty(o, 'label')).join(', ');
    } else if (nativeProperty(source, 'type') === 'file') {
      base.operations = ['upload'];
      base.value = null;
    } else {
      const fieldRole = ['textbox', 'searchbox', 'spinbutton'].includes(rname) ||
        (rname === 'combobox' && ['INPUT', 'TEXTAREA'].includes(e.tagName));
      const readOnly = !!nativeProperty(e, 'readOnly') || e.getAttribute('aria-readonly') === 'true';
      const editable = !readOnly && fieldRole;
      // A readonly field still takes a click (a datepicker opens on it) but fill is refused: say so, since the
      // click operation alone reads as an ordinary button.
      if (readOnly && fieldRole) base.read_only = true;
      // reveal() is null for <li>, <progress> and <meter>, whose numeric `value`s are not field contents.
      // A scripted value control announces its current value instead of exposing a field's value property.
      const announced = ['slider', 'spinbutton'].includes(rname)
        ? (e.getAttribute('aria-valuetext') ?? e.getAttribute('aria-valuenow')) : null;
      base.value = reveal(source) ?? announced ?? (nativeProperty(e, 'isContentEditable') || rname === 'combobox' ? innerTextOf(e).trim() : null);
      base.operations = editable ? ['fill', 'click', 'enter'] : ['click'];
      // Only an element that names itself draggable, a drawn surface to draw on, or one the page gives a grab cursor
      // is offered as a drag source: `draggable` reports true for every <a> and <img> by default, which would offer
      // the whole page as draggable. A custom widget names itself with an `aria-roledescription` (a sortable card,
      // which its pointer sensor also takes by drag), as a press is offered it; the driver reads the description and decides
      // whether to drag or press.
      if (e.getAttribute('draggable') === 'true' || base.drawn || draggedByPointer(e) ||
          e.hasAttribute('aria-roledescription')) base.operations.push('drag');
    }
    // Focus determines whether the widget can receive keys. A button, link, checkbox or switch answers Enter and
    // Space by activating, which a click already does, unless its page says it is something else: an
    // `aria-roledescription` (a sortable card is a "button" that Space picks up and the arrows move). Focus alone
    // does not say so: every button just clicked is focused, and each would add a press question to every step.
    const activation = ['button', 'link', 'checkbox', 'switch'].includes(rname) && !e.hasAttribute('aria-roledescription');
    if (!base.operations.includes('fill') && e.tagName !== 'SELECT' && nativeProperty(source, 'type') !== 'file' &&
        !activation && !['textbox', 'searchbox'].includes(rname) &&
        ARIA_ROLES.includes(rname) && (nativeProperty(e, 'tabIndex') >= 0 || e.hasAttribute('tabindex') || focused))
      base.operations.push('press');
    // A disabled next link is no next page, as the HTTP reader has it: GitHub's last issue page keeps one pointing past
    // the end.
    if (disabled) {
      base.disabled = true;
      base.operations = [];
      delete base.next_page;
    }
    controls.push(base);
  }

  // Controls that read the same cannot be told apart by their label, and a hint naming the right one
  // ("Add to cart under Sauce Labs Backpack") has nothing to match on. Give each twin the widest ancestor
  // that holds it and none of its twins, named by that ancestor's heading or its own first line of text.
  // Only labels that actually collide pay for it.
  const HEADINGS = 'h1,h2,h3,h4,h5,h6,[role="heading"],legend,caption,th,dt,summary';
  own.contextLines ||= new WeakMap();
  // Many controls read the same heading or month. Native text stays fresh; its normalization is pure code.
  // Consumers do not mutate these shared lines, which also serve later reads of the same text.
  const renderedLines = memo(e => {
    if (!e) return [];
    const text = innerTextOf(e) || '';
    const previous = own.contextLines.get(e);
    if (previous?.text === text) {
      previous.text = text;
      return previous.lines;
    }
    const lines = text.split('\n').map(line => line.replace(/\s+/g, ' ').trim()).filter(Boolean);
    own.contextLines.set(e, { text, lines });
    return lines;
  });
  const firstRenderedLine = e => renderedLines(e)[0] || '';

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
        (e.localName !== 'label' || !labelTarget(e)) && visibilityOf(e, { checkOpacity: true, checkVisibilityCSS: true }),
    ));
  // A card is an item, a row, or an element the page repeats beside itself with the same tag and class.
  // Each parent's children counted by tag and class once: asking every child about all its siblings is quadratic
  // in a list of thousands, and every element of the page is asked whether it is a card.
  // Siblings are read off the parent node: an element directly in a shadow root has no parent element, so its
  // siblings went uncounted and reading them through one threw and lost the whole snapshot.
  const twinKey = e => `${e.localName} ${e.getAttribute('class')}`;
  const twinCounts = memo(parent => {
    const counts = new Map();
    for (const child of parent?.children || []) counts.set(twinKey(child), (counts.get(twinKey(child)) || 0) + 1);
    return counts;
  });
  const CARD_RECORDS = 'article,li,[role="listitem"],tr,[role="row"]';
  const isCard = memo(e =>
    e.matches(CARD_RECORDS) ||
    (!!e.getAttribute('class') && twinCounts(e.parentNode).get(twinKey(e)) > 1));
  const cardCount = memo(parent => [...(parent?.children || [])].filter(isCard).length);
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
      const text = firstRenderedLine(title);
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
    // A line kept though other controls share it, as a commenter's name link is under each of their comments, does
    // not name the card alone: an "Edit" link in one card of several would name that card "Edit" and drop the
    // product it edits. The card's lines run on to its first one no control shares, so "Edit" and "Delete" links
    // ahead of a product's name keep it beside them.
    const lines = linesOf(scope, element, label, own);
    const named = lines.findIndex(line => !repeated.has(line));
    return (named < 0 ? lines.slice(0, 2) : lines.slice(0, named + 1)).join(' / ');
  };
  // The lines of `scope` that can name `element`, in reading order.
  const linesOf = (scope, element, label, own = null) => {
    // A line that is another control of the same kind names that control, not the section: a calendar's day cells were
    // named "31" and "7" after their neighbours in the same week row, so a picker showing November was offered with
    // contexts that pointed at other days. A card's title keeps its line, since the control it names there is another
    // kind (a link inside a checkbox's label names the item), even when it repeats: a comment author's name is a link,
    // and cutting it because the author wrote twice named every Reply by the comment's age (#881). Past its peers'
    // labels, a datepicker's first line is the month it shows. The label is cut out as whole words, since a day labelled "2" cut out of
    // "October 2026" left "October 0 6".
    const mine = controlByElement.get(element);
    const counts = labelsByRole.get(mine?.role);
    const peer = word => (counts?.get(word) || 0) > Number(mine?.label === word);
    // A table row's cells are one line of text, so a week row reads "26 27 28 29 30 1 2": a line made only of this
    // control's and its peers' labels names nothing but them.
    const onlyPeers = line => line.split(' ').every(word => word === label || peer(word));
    const lines = renderedLines(scope)
      .filter(line => !peer(line) && !onlyPeers(line) && !own?.has(line));
    return lines.map(line => (label ? ` ${line} `.split(` ${label} `).join(' ') : line).trim()).filter(Boolean);
  };
  const scopeOf = (element, holding) => {
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
    return scope;
  };
  const contextOf = (element, holding, label, nearest = true) => {
    const scope = scopeOf(element, holding);
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
    const r = boxOf(e),
      x = r.x + r.width / 2,
      y = r.y + r.height / 2;
    return r.width <= 0 || r.height <= 0 || x < 0 || x >= innerWidth || r.height > innerHeight ? null : y;
  };
  const offerHover = (e, y) => {
    const image = e.querySelector('img[alt]');
    const label =
      e.getAttribute('aria-label') ||
      image?.getAttribute('alt') ||
      firstLine(innerTextOf(e)) ||
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
        hosts = renderFact('query', doc, base);
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
          hidden = [...renderFact('query', e, tail)].some(
            t => !visible(t) && (t.textContent.trim() || t.querySelector('img,a')),
          );
        } catch {
          continue;
        }
        // Removing the active hover target renumbers its identical siblings and sends the next choice
        // to a different element. Keep it offered while the pointer still reveals its content.
        if (!hidden && !renderFact('state', e, ':hover')) continue;
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
  // Looked up per hovered card: a scan of every control for each card was quadratic on a long feed.
  for (const c of controls) {
    const element = registry.nodes.get(c.id);
    if (!controlByElement.has(element)) controlByElement.set(element, c);
  }
  // A disabled control is listed but offers nothing, so a card holding only one still has nothing to act on.
  const holding = new Set();
  for (const e of offered) {
    if (controlByElement.get(e)?.disabled) continue;
    for (let p = e?.parentElement; p && !holding.has(p); p = p.parentElement) holding.add(p);
  }
  for (const doc of docs) {
    let host = null;
    for (const e of indexedElements(doc.body)) {
      if (host?.contains(e)) continue;
      if (offered.has(e)) {
        // A card that is itself a clickable control still reveals its buttons only under the pointer: the control
        // gains Hover beside Click, as one choice rather than a second control for the same element.
        if (hoverListened(e)) {
          const own = controlByElement.get(e);
          // A disabled control offers nothing, Hover included.
          if (own && !own.disabled && !own.operations.includes('hover')) own.operations.push('hover');
        }
        continue;
      }
      if (!isCard(e)) continue;
      // A card whose script listens for the pointer entering it shows something under it, whatever else it holds:
      // a gallery card keeps a drag handle on screen and renders its delete button only while hovered.
      if (!hoverListened(e)) {
        if (holding.has(e) || ![...descendants(e, VISUAL)].some(v => !decorative(v))) continue;
      }
      const y = pointerBox(e);
      if (y === null) continue;
      offerHover(e, y);
      host = e;
    }
  }

  // The card each twin or unnamed control belongs to, where a picture may be all that tells it apart.
  const regions = new Map();
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
    // Two comments by one author posted at the same time begin with the same lines, so their Reply buttons were named
    // alike and a reply went under the wrong comment (#881). A twin whose context another twin shares adds the first
    // line of its own card that none of those twins' cards shows, its own text.
    const lines = group.map((c, i) => {
      const scope = scopeOf(twins[i], holding);
      regions.set(c.id, scope || twins[i]);
      return scope ? linesOf(scope, twins[i], c.label) : [];
    });
    const told = group.map((c, i) => {
      const alike = group.map((d, j) => j).filter(j => j !== i && c.context && group[j].context === c.context);
      if (!alike.length) return c.context;
      const others = new Set(alike.flatMap(j => lines[j]));
      const line = lines[i].find(line => line !== c.context && !others.has(line));
      return line ? `${c.context} / ${line}` : c.context;
    });
    group.forEach((c, i) => {
      if (told[i]) c.context = told[i];
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
        if (!visibilityOf(node.parentElement, { checkOpacity: true, checkVisibilityCSS: true })) continue;
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
  const fieldLines = element =>
    element.tagName === 'SELECT' || FIELD_CONTAINERS.has(element.getAttribute('role')) ? new Set(renderedLines(element)) : null;
  const nearestName = (element, label) => {
    const own = fieldLines(element);
    let first = '';
    for (let e = element.parentElement, path = element; e && e !== e.ownerDocument.body; path = e, e = e.parentElement) {
      first ||= nameOf(e, element, label, true, own);
      if (!isCard(e)) continue;
      // An action row's own caption ("Actions") is the first text the climb meets and names no item, so the card
      // is named by what it holds outside the row that holds the control.
      const aria = (e.getAttribute('aria-label') || '').trim();
      const outside = [...e.children]
        .filter(child => child !== path)
        .map(child => firstRenderedLine(child))
        .find(line => line && line !== label && !repeated.has(line));
      // A card's own heading names it before its first line, which can be another control: an OpenLayers example's
      // block opens with its "Edit" link, and the map drawn under the heading "Icon Symbolizer" was named "Edit".
      return (aria !== label && aria) || sectionOf(e, element, label, false) || outside || first;
    }
    return first;
  };
  for (const c of controls) {
    if (!c.context && unnamed.has(c.id)) {
      const context = nearestName(registry.nodes.get(c.id), c.label);
      if (context) c.context = context;
    }
  }

  // A twin, or a control with no name of its own, whose card shows a picture: the text names it by what surrounds it,
  // but the task may name it by what the picture shows, a product photo beside one of six "Add" buttons or the icon
  // that is an unnamed button's only face. It is marked so the step can show that card to Jev as pixels.
  registry.pictured = new Map();
  // A box with no text that paints a background shows what only its pixels say: a shop's colour swatch is an empty
  // span filled green, and its radio, named only "radio", was told from the three beside it by nothing.
  // A field that shows a typed or chosen value fills its box with the browser's own field colour, which pictures
  // nothing: a page repeating a booking form had every field marked as pictured.
  const SHOWS_VALUE =
    'textarea,select,input:not([type=button],[type=submit],[type=reset],[type=image],[type=radio],[type=checkbox],[type=color])';
  const painted = e => {
    if (e.textContent.trim() || e.matches(SHOWS_VALUE) || decorative(e) || !visible(e)) return false;
    const style = getComputedStyle(e);
    return style.backgroundImage !== 'none' || style.backgroundColor !== 'rgba(0, 0, 0, 0)';
  };
  for (const c of controls) {
    const element = registry.nodes.get(c.id);
    if (unnamed.has(c.id) && !regions.has(c.id)) {
      let card = element;
      for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
        if (isCard(e)) {
          card = e;
          break;
        }
      }
      regions.set(c.id, card);
    }
    const region = regions.get(c.id);
    if (!region) continue;
    // A shape drawn inside an SVG is its own picture: Leaflet's circle and triangle are two unnamed paths that only
    // their pixels tell apart.
    const pictures = [...(region.matches(VISUAL) || region.ownerSVGElement ? [region] : []), ...descendants(region, VISUAL)];
    if (!pictures.some(v => !decorative(v) && visible(v)) && ![region, ...descendants(region, '*')].some(painted)) continue;
    c.pictured = true;
    registry.pictured.set(c.id, region);
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
  // A comments section's own "Comment text area" sits before the list of comments and inside none of them, yet got
  // only the section's heading, the same context every comment's own Reply has, so a task to reply to one comment
  // filled the page's new-comment box (#1476). The DOM states whether a control is in a record: a record is what
  // `scopeOf` already stops at (an article or list item), and a section lists records when at least two hold a control.
  // A record also holds text of its own beside its controls: an editor's toolbar is a list whose items each hold one
  // button and nothing else, and counted as records it named the editor's Save "outside every item listed".
  const RECORD = 'article,li,[role="listitem"]';
  // Built on first use: most captures name every control without asking whether a section lists records.
  let controlNodes = null;
  let recordsHoldingControls = null;
  const holdingControls = record => {
    if (!recordsHoldingControls) {
      controlNodes = new Set(controls.map(c => registry.nodes.get(c.id)));
      recordsHoldingControls = new Set();
      for (const e of controlNodes) for (let r = e?.closest(RECORD); r; r = r.parentElement?.closest(RECORD)) recordsHoldingControls.add(r);
    }
    return recordsHoldingControls.has(record);
  };
  const holdsOwnText = memo(record => {
    const texts = record.ownerDocument.createTreeWalker(record, NodeFilter.SHOW_TEXT);
    for (let node = texts.nextNode(); node; node = texts.nextNode()) {
      if (!node.data.trim()) continue;
      let inControl = false;
      for (let e = node.parentElement; e && !inControl; e = e === record ? null : e.parentElement) inControl = controlNodes.has(e);
      if (!inControl) return true;
    }
    return false;
  });
  const recordCount = memo(section =>
    [...descendants(section, RECORD)].filter(r => holdingControls(r) && holdsOwnText(r)).length);
  // Another record is a card that holds both an owner (its first line, when no control of its own says it) and
  // controls of its own beyond that line, and that has a sibling card doing the same: replies listed under a comment
  // each name their author and carry their own buttons, whatever their markup or classes. A layout row holds one or
  // the other (an author row is a bare link, an actions row sits beside it, an editor row holds only its field or its
  // Post button), so a comment's rows and a form's rows never pair up.
  const isRecordCard = memo(card => {
    const [first] = renderedLines(card);
    if (!first || !isCard(card)) return false;
    return controls.some(c => c.label !== first && card.contains(registry.nodes.get(c.id)));
  });
  const isListedRecord = card =>
    isRecordCard(card) && [...card.parentNode.children].some(c => c !== card && isRecordCard(c));
  const listsRecordsOutside = (section, element) => {
    if (recordCount(section) < 2) return false;
    // A class-styled comment card is a record too: its own controls are named by it, not outside every record. A
    // page-level comment form laid out in repeated rows is not: those rows are cards, but no record, and counting them
    // took the note that the form sits outside every comment off its field and its Post button.
    for (let e = element; e !== section; e = e.parentElement) if (e.matches(RECORD) || isListedRecord(e)) return false;
    // The section is the control's own record only when it is one of a list: a comment whose replies are listed inside
    // it names its own Reply plainly, while a lone post that holds its comments form and comments list is the page's
    // article, and its form sits outside every comment listed in it.
    return !isListedRecord(section);
  };
  // The section's name, and whether the control sits outside every record the section lists.
  const nearestHeading = (element, label) => {
    for (let inner = element, e = element.parentElement; e && e !== e.ownerDocument.body; inner = e, e = e.parentElement) {
      const aria = (e.getAttribute('aria-label') || '').trim();
      if (aria && aria !== label) return { name: aria, outside: listsRecordsOutside(e, element) };
      let first = '',
        before = '';
      for (const title of openTitles(e)) {
        if (inner.contains(title)) continue;
        const text = firstRenderedLine(title);
        if (!text || text === label) continue;
        first ||= text;
        if (title.compareDocumentPosition(element) & Node.DOCUMENT_POSITION_FOLLOWING) before = text;
      }
      const name = before || first;
      if (name) return { name, outside: listsRecordsOutside(e, element) };
    }
    return { name: '' };
  };
  // A control whose label is its own can still belong to one record of several: a comment's "Hide 3 Replies" sits in
  // its list item beside the comment card, so it never met a twin, took the section heading "Comments" from the
  // fallback below, and a run read another comment's counter as the count of the replies it asked about (#1467). Such
  // a control is named by the nearest card that repeats among its siblings (a list item, a row, or a class-styled
  // card), even when it is the card's only control. Card counts are kept per parent: a sibling scan per control was
  // quadratic on a long list.
  const recordOf = memo(element => {
    // A lone card inside a repeating one is still a record of its own: the only reply under a comment is named by
    // that reply, not by the comment it answers, so it is kept while the climb looks for the repeating card around it.
    let lone = null;
    for (let e = element.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      if (!isCard(e)) continue;
      if (cardCount(e.parentNode) > 1) return lone || e;
      lone ||= e;
    }
    return null;
  });
  // The repeating card around a record, for a record that names nothing.
  const recordAbove = card => {
    for (let e = card.parentElement; e && e !== e.ownerDocument.body; e = e.parentElement) {
      if (isCard(e) && cardCount(e.parentNode) > 1) return e;
    }
    return null;
  };
  // A card names a control by a title of its own. A comment's layout rows repeat their class inside every comment and
  // hold a bare line ("Actions") where the comment holds its author's heading, so a card with no title leaves the name
  // to the repeating record around it; only when none has a title does the innermost record's first line name it.
  const titleOf = (card, element, label) => {
    const aria = (card.getAttribute('aria-label') || '').trim();
    return aria && aria !== label ? aria : sectionOf(card, element, label, true);
  };
  for (const c of controls) {
    if (c.context) continue;
    const element = registry.nodes.get(c.id);
    const record = recordOf(element);
    if (!record) continue;
    let context = '',
      owner = record;
    // A card holding another listed record's controls is a different comment: neither the title climb nor the owner
    // climb may step out into it, or a reply would take its parent's heading while its sibling replies share the same one.
    const holdsOtherCard = (above, own) => {
      for (const card of above.querySelectorAll('*')) if (!card.contains(own) && !own.contains(card) && isListedRecord(card)) return true;
      return false;
    };
    // A lone record is a record of its own (the only reply names itself, not the comment it answers); only a repeating
    // card can be a layout row that defers to the record around it.
    const above = card => (cardCount(card.parentNode) > 1 ? recordAbove(card) : null);
    for (let card = record; card && !context; ) {
      context = titleOf(card, element, c.label);
      const next = above(card);
      card = next && !holdsOtherCard(next, card) ? next : null;
    }
    // No card has a title, so a first line names it. A comment that names its author in bold or by a link has no
    // heading: its counter read the layout row's "Actions" while the Reply beside it read the author. The record is
    // the widest card around the counter that holds no other card with controls, the card the twin path reaches for a
    // Reply: the author's own link or text sits in the comment beside its rows.
    for (let next = cardCount(record.parentNode) > 1 ? recordAbove(owner) : null; next && !holdsOtherCard(next, owner); next = recordAbove(next)) owner = next;
    context ||= nameOf(owner, element, c.label, true, fieldLines(element));
    if (context) c.context = context;
  }
  // A twin already has a context that tells it from its twins, but a reply editor opening beside the page's own box
  // made both labels twins and the box lost the note that it sits outside the comments, so it keeps that note too.
  for (const c of controls) {
    const { name, outside } = nearestHeading(registry.nodes.get(c.id), c.label);
    if (!name) continue;
    const note = outside ? `${name}, outside every item listed under ${name}` : name;
    if (!c.context) c.context = note;
    else if (outside) c.context = `${note} / ${c.context}`;
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

  // A tab or option inherits its key direction from the composite that owns it, not from its own role.
  for (const c of controls) {
    if (!c.operations.includes('press')) continue;
    const e = registry.nodes.get(c.id);
    const owner = e?.closest('[role="tablist"],[role="listbox"],[role="radiogroup"],[role="menu"],[role="menubar"],[role="tree"],[role="treegrid"],[role="grid"]');
    const orientation = e?.getAttribute('aria-orientation') ?? owner?.getAttribute('aria-orientation');
    if (orientation === 'vertical' || orientation === 'horizontal')
      c.context = [c.context, `orientation: ${orientation}`].filter(Boolean).join('; ');
  }

  // A popup a page appends to <body> (a datepicker, a menu, a combobox's list) is drawn beside the control that opened
  // it but comes last in reading order, under whatever section ends the page: a driver read a datepicker popup's
  // December as the inline calendar's below it (#1538). A layer is an element taken out of flow (absolute or fixed),
  // positioned against the page rather than inside a component (a card's badge is positioned against its card), and
  // drawn over something that is not its own ancestor, as a hit test at the centre of its visible part shows.
  const drawnOver = e => {
    const box = boxOf(e);
    const view = e.ownerDocument.defaultView;
    const left = Math.max(box.left, 0), right = Math.min(box.right, view.innerWidth);
    const top = Math.max(box.top, 0), bottom = Math.min(box.bottom, view.innerHeight);
    if (right <= left || bottom <= top) return false;
    const stack = renderFact('stack', e.ownerDocument, [(left + right) / 2, (top + bottom) / 2]);
    const own = stack.findIndex(hit => e.contains(hit));
    return own >= 0 && stack.slice(own).some(hit => !e.contains(hit) && !hit.contains(e));
  };
  const layerOf = memo(e => {
    if (!e || e === e.ownerDocument.body || e === e.ownerDocument.documentElement) return null;
    const position = styleProperty(e, 'position');
    if (position === 'absolute' || position === 'fixed') {
      const against = nativeProperty(e, 'offsetParent');
      if ((!against || against === e.ownerDocument.body || against === e.ownerDocument.documentElement) && drawnOver(e))
        return e;
    }
    return layerOf(e.parentElement ?? e.getRootNode().host);
  });
  const layers = [];
  for (const c of controls) {
    const layer = layerOf(registry.nodes.get(c.id));
    if (!layer) continue;
    if (!layers.includes(layer)) layers.push(layer);
    c.layer = layers.indexOf(layer) + 1;
  }

  // "First" and "last" are places in a list, which a label does not state. The page's own `aria-posinset` and
  // `aria-setsize` say it where present; otherwise the place among the visible items of the nearest list container.
  // A custom widget's items name themselves with an `aria-roledescription` (a sortable card, a slide) and sit in a plain
  // container, whatever the page calls them or in which language. A tree's child group is its own set.
  const SORTABLE = '[aria-roledescription]';
  const LIST_ITEM = 'li,[role="listitem"],[role="option"],[role="row"],[role="treeitem"]';
  const SET_ITEM = `${LIST_ITEM},${SORTABLE}`;
  const SET_LIST = 'ul,ol,[role="list"],[role="listbox"],[role="grid"],[role="tree"],[role="tree"] [role="group"]';
  // A list item that holds a described control is the member, not the control inside it.
  const PRESSED = `${SORTABLE}[aria-pressed="true"]`;
  const setOwner = i => (i.matches(SORTABLE) && !i.matches(LIST_ITEM) ? i.parentElement : i.parentElement?.closest(SET_LIST));
  for (const c of controls) {
    const e = registry.nodes.get(c.id);
    const item = e?.closest(`[aria-posinset],${LIST_ITEM}`) ?? e?.closest(SORTABLE);
    if (!item) continue;
    const list = setOwner(item);
    const members = list
      ? [...list.querySelectorAll(SET_ITEM)].filter(i => setOwner(i) === list && visible(i))
      : [];
    // A picked-up sortable card moves by transform while the DOM keeps its order, and the library states its order
    // nowhere else a page reliably carries (its live announcement is prose in the page's language), so the order a
    // person sees is the one stated: otherwise every arrow press leaves the page looking unchanged. Neighbours are read
    // at the slots the layout gave them, since they slide to new ones over an animation. The held item is drawn at its
    // own slot moved by every translation between the pressed handle and the item, wherever the page put it.
    // Rows and columns are told apart by centres, since the edges of two cells that touch are read from different
    // sources (a box, a transform string) and do not compare exactly. The item has passed a neighbour in a row before
    // its own, or a neighbour beside it in its row on the side it moved to; where it covers a neighbour's centre, it has
    // passed that neighbour once its edge leading on the axis it travelled reaches that centre, so a tall card seated by
    // the edge it leads with counts, and a wrapping grid's slots run in rows.
    const held = members.find(i => i.matches(PRESSED) || i.querySelector(PRESSED));
    let siblings = members;
    if (held) {
      // A library slides the lifted item to its new place over a transition, and the page is read while it runs: the
      // place it holds is where the transition ends, not the frame it is passing through.
      const settled = e =>
        e.getAnimations().map(a => a.effect?.getKeyframes().at(-1)?.transform).filter(Boolean).at(-1) ??
        styleProperty(e, 'transform');
      // A translation is in its parent's units while boxes are in the viewport's, so a scaled list or grid (a zoomed
      // board) would otherwise compare a full slot's move against half a slot. The scale each element's parent draws
      // it at is the product of every ancestor's.
      const scaleAbove = e => {
        let x = 1;
        let y = 1;
        for (let p = e.parentElement; p; p = p.parentElement) {
          const m = new DOMMatrix(styleProperty(p, 'transform'));
          x *= m.a;
          y *= m.d;
        }
        return [x, y];
      };
      const moved = (from, to) => {
        let x = 0;
        let y = 0;
        for (let e = from; e; e = e.parentElement) {
          const m = new DOMMatrix(settled(e));
          const [sx, sy] = scaleAbove(e);
          x += m.m41 * sx;
          y += m.m42 * sy;
          if (e === to) break;
        }
        return [x, y];
      };
      // The box the layout gave an item, before its own transform: a library may also scale a lifted card to the
      // slot it hovers, and a scale about the transform origin moves every edge, so the scale is undone as well as the
      // translation. Sortable libraries scale and translate; they do not rotate.
      const slot = (e, [dx, dy] = [0, 0]) => {
        const box = boxOf(e);
        const m = new DOMMatrix(styleProperty(e, 'transform'));
        const [ox, oy] = styleProperty(e, 'transformOrigin').split(' ').map(Number.parseFloat);
        const [sx, sy] = scaleAbove(e);
        const left = box.left - sx * (m.e + ox * (1 - m.a)) + dx;
        const top = box.top - sy * (m.f + oy * (1 - m.d)) + dy;
        return { left, top, right: left + box.width / m.a, bottom: top + box.height / m.d };
      };
      const shift = moved(held.matches(PRESSED) ? held : held.querySelector(PRESSED), held);
      const vertical = Math.abs(shift[1]) >= Math.abs(shift[0]);
      const drawn = slot(held, shift);
      const mid = box => ({ x: (box.left + box.right) / 2, y: (box.top + box.bottom) / 2 });
      const above = (a, b) => mid(a).y < b.top && mid(b).y > a.bottom;
      const side = (a, b) => (mid(a).x < b.left ? -1 : mid(a).x > b.right ? 1 : 0);
      const reaches = (j, forward) =>
        vertical ? (forward ? drawn.bottom >= mid(j).y : drawn.top <= mid(j).y)
          : forward ? drawn.right >= mid(j).x : drawn.left <= mid(j).x;
      const passed = (j, forward) =>
        forward
          ? above(j, drawn) || (!above(drawn, j) && (side(j, drawn) < 0 || (side(j, drawn) === 0 && reaches(j, true))))
          : above(drawn, j) || (!above(j, drawn) && (side(j, drawn) > 0 || (side(j, drawn) === 0 && reaches(j, false))));
      const at = members.indexOf(held);
      const after = members.filter((i, k) => k > at && passed(slot(i), true)).length;
      const before = members.filter((i, k) => k < at && passed(slot(i), false)).length;
      siblings = members.filter(i => i !== held);
      siblings.splice(at + after - before, 0, held);
    }
    const declared = Number.parseInt(item.getAttribute('aria-posinset'), 10);
    // `aria-setsize="-1"` says the list's size is unknown (a virtualised feed); counting the rendered items instead
    // would state a size the page says it does not know.
    const declaredSize = item.getAttribute('aria-setsize');
    const size = declaredSize === null ? siblings.length : Number.parseInt(declaredSize, 10);
    const position = declared > 0 ? declared : siblings.indexOf(item) + 1;
    const count = size > 0 ? size : 0;
    if (position > 0 && count > 1 && position <= count) {
      c.item_position = position;
      c.item_count = count;
    }
  }

  // A wheel at the viewport centre misses panels and horizontal lists. Index the browser's actual scroll owners,
  // separately for each overflowing axis, so the chosen target determines which content moves.
  registry.scrollIds ||= new WeakMap();
  registry.scrollAxes ||= new Map();
  const scrollAreas = root => {
    for (const e of indexedElements(root)) {
      if (e.shadowRoot) scrollAreas(e.shadowRoot);
      if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
        const inner = childDocument(e);
        if (inner?.body && granted(inner)) scrollAreas(inner);
      }
      if (!safe(e) || !visible(e) || e.closest('[inert]')) continue;
      const view = e.ownerDocument.defaultView;
      const page = e === e.ownerDocument.scrollingElement;
      const [scrollWidth, clientWidth, scrollHeight, clientHeight] = renderFact('scrollSize', e);
      const overflow = {x: scrollWidth - clientWidth, y: scrollHeight - clientHeight};
      if (overflow.x <= 0 && overflow.y <= 0) continue;
      const [writingMode, direction, display, flexDirection, overflowX, overflowY] = renderFact('scrollStyle', e);
      const vertical = writingMode.startsWith('vertical') || writingMode.startsWith('sideways');
      let inlineReverse = direction === 'rtl';
      let blockReverse = vertical && writingMode.endsWith('-rl');
      if (display.includes('flex')) {
        if (flexDirection === 'row-reverse') inlineReverse = !inlineReverse;
        if (flexDirection === 'column-reverse') blockReverse = !blockReverse;
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
        let behavior = axis === 'x' ? overflowX : overflowY;
        // With visible root overflow, CSS propagates the body's overflow to the viewport, including modal locks.
        if (page && behavior === 'visible' && e.ownerDocument.body) {
          behavior = styleProperty(e.ownerDocument.body, axis === 'x' ? 'overflowX' : 'overflowY');
        }
        if (overflow[axis] <= 0 || !(page && behavior === 'visible') && !['auto', 'scroll', 'overlay'].includes(behavior)) continue;
        const position = renderFact('scrollPosition', e)[axis === 'x' ? 0 : 1];
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
        const rect = boxOf(e);
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
      const f = boxOf(frame);
      // The child document starts inside the frame's border and padding.
      const left = nativeProperty(frame, 'clientLeft') + parseFloat(styleProperty(frame, 'paddingLeft'));
      const top = nativeProperty(frame, 'clientTop') + parseFloat(styleProperty(frame, 'paddingTop'));
      read(inner, dx + f.left + left, dy + f.top + top);
    }
  };
  // The box a text node's line is laid out in: the nearest ancestor that is not inline, as capture.js splits blocks.
  // Text nodes in one box read as one line, the way the page shows them. One line per node showed React's split
  // `Total: $` and `43.18` on two lines, a text no capture held, and every quote of saucedemo's total was refused.
  const lineBox = memo(e => {
    for (let at = e; at; at = at.parentElement) {
      const display = styleProperty(at, 'display');
      if (!(display.startsWith('inline') || display === 'contents')) return at;
    }
    return null;
  });
  // Spaces collapse across the nodes of a line too: a card's icon button between two gaps showed one space, not two.
  const joined = (line, value) => (line.endsWith(' ') && value.startsWith(' ') ? line + value.slice(1) : line + value);
  // CSS strings as capture.js reads them (its `cssStrings`), so the text the driver quotes is the text its quotes are
  // checked against: Google Flights draws "JetBlue · Tap Air Portugal" with the dot as generated content, the capture
  // held the dot and this text did not, and each quote of "JetBlueTap Air Portugal" was refused. Counters, images and
  // other functions are not rendered strings; an empty alternative marks the content as decoration.
  const cssStrings = content => {
    const strings = /"((?:[^"\\]|\\.)*)"\s*/gy;
    let value = '';
    while (strings.lastIndex < content.length) {
      const at = strings.lastIndex;
      const match = strings.exec(content);
      if (!match) {
        if (content[at] === '/') return cssStrings(content.slice(at + 1).trim()) ? value : null;
        const token = /[-\w]+/y;
        token.lastIndex = at;
        if (!token.exec(content)) return null;
        let end = token.lastIndex;
        if (content[end] === '(') {
          let depth = 0;
          let quote = '';
          do {
            const char = content[end++];
            if (char === '\\') end++;
            else if (quote) {
              if (char === quote) quote = '';
            } else if (char === '"' || char === "'") quote = char;
            else if (char === '(') depth++;
            else if (char === ')') depth--;
          } while (end < content.length && (depth || quote));
          if (depth || quote) return null;
        }
        while (/\s/.test(content[end] ?? '') && end < content.length) end++;
        strings.lastIndex = end;
        continue;
      }
      value += match[1].replace(/\\([\da-fA-F]{1,6}[ \t\r\n\f]?|.)/g, (_, escaped) => {
        if (!/^[\da-fA-F]/.test(escaped)) return escaped;
        const code = parseInt(escaped.trim(), 16);
        return String.fromCodePoint(code === 0 || code > 0x10ffff || (code >= 0xd800 && code <= 0xdfff) ? 0xfffd : code);
      });
    }
    return value;
  };
  // An element's ::before and ::after text as capture.js's `generatedOf` reads it: a block one is a line of its own,
  // an inline one keeps the gap its margins or its host's margins draw. An icon font's glyph is named elsewhere.
  const generatedOf = memo(e => {
    const read = ([display, visibility, content, start, end]) => {
      if (display === 'none' || visibility !== 'visible') return '';
      const value = cssStrings(content);
      if (!value) return '';
      if (!(display.startsWith('inline') || display === 'contents')) return {block: value};
      return `${parseFloat(start) > 0 ? ' ' : ''}${value}${parseFloat(end) > 0 ? ' ' : ''}`;
    };
    // Most elements draw none, so the pseudo-elements are read before the host's own tests.
    const facts = renderFact('pseudo', e);
    const found = [read(facts.slice(0, 5)), read(facts.slice(5))];
    if (!found.some(Boolean)) return ['', ''];
    if (e.getAttribute('aria-hidden') === 'true' || [...e.classList].some(token => {
      const glyph = ICON_CLASS.exec(token);
      return glyph && !ICON_STYLE.test(glyph[1]);
    })) return ['', ''];
    // The generated text's host can carry the gap instead, on either side of what it holds.
    if (typeof found[0] === 'string' && parseFloat(styleProperty(e, 'marginInlineStart')) > 0) found[0] = ` ${found[0]}`;
    if (typeof found[1] === 'string' && parseFloat(styleProperty(e, 'marginInlineEnd')) > 0) found[1] += ' ';
    return found;
  });
  // Size, the cheapest test, goes first.
  const readText = (doc, dx, dy) => {
    if (!doc.body) return;
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT);
    // The line being read: its list, its box and its place in the list. A <br> ends it.
    let run = null;
    // Generated text joins the line it is drawn in; a block of it is a line of its own in the list being read.
    const generate = (value, element) => {
      if (!value) return;
      if (typeof value !== 'string') {
        const r = boxOf(element);
        const list = r.top + dy + r.height > 0 && r.top + dy < innerHeight ? words : offscreen;
        list.push(value.block);
        run = null;
        return;
      }
      const box = lineBox(element);
      if (run && run.box === box) {
        run.list[run.at] = joined(run.list[run.at], value);
        return;
      }
      const r = boxOf(element);
      const list = r.top + dy + r.height > 0 && r.top + dy < innerHeight && r.left + dx + r.width > 0 &&
        r.left + dx < innerWidth ? words : offscreen;
      run = {list, box, at: list.push(value) - 1};
    };
    // The ::after text of the elements the walk is inside, written once the walk leaves each one.
    const after = [];
    const leave = node => {
      while (after.length && !after.at(-1)[0].contains(node)) {
        const [element, value] = after.pop();
        generate(value, element);
      }
    };
    let node;
    while ((node = walker.nextNode())) {
      leave(node);
      if (node.nodeType === Node.ELEMENT_NODE) {
        if (node.tagName === 'BR') run = null;
        if (!CODE.has(node.tagName)) {
          const [before, end] = generatedOf(node);
          // Visibility is the dearer test, so only an element that draws generated text asks it.
          if ((before || end) && visible(node)) {
            generate(before, node);
            if (end) after.push([node, end]);
          }
        }
        continue;
      }
      // Whitespace collapses as CSS lays out normal text: a gap between two inline nodes is one space of the line.
      // Visible text is read whole on every observe, as it always was; the context lines' cache counts its own
      // `replace` normalizations of unchanged text, so this pass does not go through `replace`.
      const value = node.textContent.split(/\s+/).join(' '),
        parent = node.parentElement;
      if (!parent) continue;
      if (!value.trim()) {
        if (run && lineBox(parent) === run.box) run.list[run.at] = joined(run.list[run.at], ' ');
        continue;
      }
      const r = textBoxOf(node);
      if (r.width <= 0 || r.height <= 0 || parent.closest('script,style,noscript,template') || !visible(parent)) {
        continue;
      }
      const top = r.top + dy,
        left = r.left + dx;
      const list =
        top + r.height > 0 && top < innerHeight && left + r.width > 0 && left < innerWidth ? words : offscreen;
      const box = lineBox(parent);
      if (run && run.list === list && run.box === box) {
        list[run.at] = joined(list[run.at], value);
      } else {
        run = {list, box, at: list.push(value) - 1};
      }
    }
    leave(null);
    readFrames(doc, dx, dy, readText);
  };
  readText(document, 0, 0);
  const viewport_text = words.map(line => line.trim()).join('\n');
  const offscreen_text = offscreen.map(line => line.trim()).join('\n');

  // Page state carried by pixels: a chart, a picture, a drawing or a video is evidence the page text and the
  // control list cannot hold, and the verifier needs the screenshot for it. Only what the viewport shows counts,
  // because that is what a screenshot captures. Geometry is read in the parent's coordinates the way readText reads
  // text, so a frame's image counts where the frame itself sits on screen.
  let visual_content = 0;
  const readVisual = (doc, dx, dy) => {
    if (visual_content) return;
    for (const e of doc.querySelectorAll(VISUAL)) {
      if (decorative(e) || !visible(e)) continue;
      const r = boxOf(e);
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
        label: (e.hasAttribute('aria-label') || e.hasAttribute('aria-labelledby') ? labelOf(e) : nativeProperty(e, 'title')) || null,
        heading: headingBefore(e) || null,
        paused: nativeProperty(e, 'paused'),
        ended: nativeProperty(e, 'ended'),
        muted: nativeProperty(e, 'muted'),
        elapsed_seconds: seconds(nativeProperty(e, 'currentTime')) ?? 0,
        duration_seconds: seconds(nativeProperty(e, 'duration')),
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
    // A redraw changes the node ids at either end of the guard. Everything between them but the control's own state,
    // including the receiving document's timeOrigin and form semantics, must survive before an action can follow it.
    const retarget = registry.retargetKey(registry.nodes.get(c.id), c.id);
    retarget_keys[c.id] = retarget && digest(JSON.stringify(retarget));
  }
  // Kept so a later check can ask whether any of these controls changed without sending the guards back.
  if (mode !== 'glance') registry.observed = new Map(controls.map(c => [c.id, guards[c.id]]));
  const field_state = [...document.querySelectorAll('input,textarea,select')]
    .filter(safe)
    .map(e => [identity(e), reveal(e), nativeProperty(e, 'checked'), chosen(e), nativeProperty(e, 'disabled'), nativeProperty(e, 'readOnly')]);
  // Compare meaning and identity for the page_key fingerprint; geometry is re-resolved just before input.
  const semantics = controls.map(({ distance, ...c }) => c);
  // A scroll can land on a new part without changing text, while live text can change on the same part. Keep this
  // identity to let the agent distinguish those cases without treating page content as geometry.
  // A scroll area is named by its label and its place among areas of that label, never by its registry id: a page
  // that replaces the container while rendering would otherwise turn a place already shown into a new one.
  const seen_areas = new Map();
  const scroll_positions = controls
    .filter(c => registry.scrollAxes?.has(c.id))
    .map(c => {
      const e = registry.nodes.get(c.id);
      const nth = seen_areas.get(c.label) ?? 0;
      seen_areas.set(c.label, nth + 1);
      return [c.label, nth, ...renderFact('scrollPosition', e)];
    });
  const viewport_key = digest(JSON.stringify([
    location.href,
    scrollX,
    scrollY,
    innerWidth,
    innerHeight,
    scroll_positions,
  ]));
  // A selection is what the next key acts on, so an action planned on one is not run on a page without it.
  const selection = renderFact('selection', document);
  const page_key = digest(JSON.stringify([
    location.href,
    scrollX,
    scrollY,
    innerWidth,
    innerHeight,
    document.title,
    selection,
    viewport_text,
    semantics,
    field_state,
    controls.filter(c => registry.scrollAxes?.has(c.id)).map(c => {
      const e = registry.nodes.get(c.id);
      return [c.id, ...renderFact('scrollPosition', e)];
    }),
    // Playing or not changes the page; the clock advancing does not, or no key would hold until an action.
    media.map(({ elapsed_seconds, ...m }) => m),
  ]));

  // The open modal the rest of the page sits behind. A page marks what is under one aria-hidden or inert, so those
  // controls leave the observation while staying in the document: Google Flights' Stops filter took 145 controls
  // out and put them back on closing, and a run told each by name in every later step.
  // Most modals never say aria-modal and need no dialog role: MUI, Radix, react-aria and Headless UI mark the page
  // behind a modal aria-hidden or inert instead ("aria-hide others"). A modal is what leaves every other control on
  // the page hidden from use, so an element is one when the controls the capture collected, which are exactly the
  // ones not hidden, all lie inside it, and some control sits in hidden content outside it. A page that hid
  // interactive content from assistive tech while it stayed usable would break ARIA, and the test says nothing
  // about how the page looks. CurseForge's "Delete media" dialog and its gallery edit drawer (a role="presentation"
  // root, no dialog role) are both this shape; unrecognised, the controls they hid were reported as removed. A
  // carousel's aria-hidden clones are not: the page's other controls stay outside the visible slide. The candidate is
  // the sibling of the outermost hidden content, so a dialog and its backdrop controls count as inside. The check is
  // one containment test per collected control. A role="dialog" that says aria-modal="false" stays non-modal.
  const hiddenSelector = '[aria-hidden="true"],[inert]';
  // A control is drawn or not by its own rendering: a wrapper with no box (display:contents) still draws its controls,
  // and a rendered wrapper can hold only a display:none or fully transparent panel.
  const holdsDrawnControl = e =>
    [e, ...e.querySelectorAll(SELECTOR)].some(c => c.matches(SELECTOR) && visibilityOf(c, { checkOpacity: true, checkVisibilityCSS: true }));
  // Element.contains stops at a shadow root, but the capture walks open ones, so containment climbs through each host.
  const containsAcrossShadow = (root, e) => {
    for (let n = e; n; n = n.getRootNode().host) if (root.contains(n)) return true;
    return false;
  };
  const dialogSelector = '[role="dialog"],[role="alertdialog"]';
  const found = new Set(renderFact('query', document, 'dialog:modal,[aria-modal="true"]'));
  const usable = controls
    .filter(c => !registry.scrollAxes?.has(c.id))
    .map(c => registry.nodes.get(c.id))
    .filter(Boolean);
  for (const hidden of renderFact('query', document, hiddenSelector)) {
    // The page behind a modal is still drawn; a menu or template kept aria-hidden while dormant holds no drawn
    // control, and would otherwise make the whole usable page look like a modal sitting over it.
    if (hidden.parentElement?.closest(hiddenSelector) || !holdsDrawnControl(hidden)) continue;
    for (const sibling of hidden.parentElement?.children || []) {
      if (sibling === hidden || sibling.matches(hiddenSelector) || !usable.length || !usable.every(e => containsAcrossShadow(sibling, e))) continue;
      const dialog = sibling.matches(dialogSelector) ? sibling : sibling.querySelector(dialogSelector);
      if (dialog?.getAttribute('aria-modal') !== 'false') found.add(sibling);
    }
  }
  const modals = [...found]
    .filter(e => !e.closest(hiddenSelector) && visibilityOf(e, { checkVisibilityCSS: true }))
    .sort((a, b) => (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
  // A dialog is named by aria-label or aria-labelledby, never by its content, on the modal or on the dialog inside it.
  const ariaName = e =>
    (
      e.getAttribute('aria-label') ||
      (e.getAttribute('aria-labelledby') || '')
        .split(/\s+/)
        .map(id => (id && innerTextOf(e.ownerDocument.getElementById(id))) || '')
        .join(' ')
    )
      .replace(/\s+/g, ' ')
      .trim();
  const dialogName = e => ariaName(e) || (e.querySelector(dialogSelector) ? ariaName(e.querySelector(dialogSelector)) : '');
  const modal = modals.length ? dialogName(modals.at(-1)) : null;

  // The read number names a transport baseline, and the read is kept to answer "unchanged" while the page stays.
  const read = ++registry.reads;
  own.renderReads ||= {};
  own.renderReads[mode] = { read, document_key: String(performance.timeOrigin) };
  // The page an observation described, in the terms a Wait compares. A Wait is decided on that page and watches the
  // page it finds when it starts: the one reader of both keeps a page that finished changing in between from being
  // taken as the page the Wait began on. Taken in this evaluation, so no change falls between the two.
  if (mode === 'snapshot') registry.observedFingerprint = takeFingerprint().fingerprint;
  registry.kept ||= {};
  registry.kept[mode] = {
    mode,
    read,
    rev: readRev,
    roots: walkedRoots,
    walked: new Set(walkedRoots.map(([root]) => root)),
    states: walkedRoots.map(([root]) => rootState(root)),
    frames: walkedFrames,
    undefinedElements: walkedRoots.flatMap(([root]) => own.queryTrees.get(root).undefinedElements),
    listenerStates: [...listenerStates],
    hoverStates: [...hoverStates],
    listenerAPI: Boolean(own.listeners),
    inputs: renderInputs,
    url: location.href,
    title: document.title,
    document_key: String(performance.timeOrigin),
    page_key,
  };
  const result = {
    url: location.href,
    title: document.title,
    viewport_text,
    viewport_key,
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
  // Absent unless text is selected, as on most pages.
  if (selection) result.selection = selection;
  // A JSON document is shown by its structure, as capture.js sends it to be read: shown Chrome's rendering of the
  // served text, the driver quoted `"size": "large"` off httpbin's echo, and the check, which reads the document key by
  // key, found no page that said it.
  const type = document.contentType.toLowerCase();
  const served = document.querySelector('body > pre');
  if (served && (type === 'application/json' || (type.startsWith('application/') && type.endsWith('+json')))) {
    result.content_type = document.contentType;
    result.json_text = served.textContent;
  }
  const previous = own.snapshots?.[mode];
  own.snapshots ||= {};
  own.snapshots[mode] = { read, document_key: result.document_key, controls: semantics, guards, retarget_keys };
  if (delta && previous?.document_key === result.document_key &&
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
