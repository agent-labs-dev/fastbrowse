// The agent cursor fastbrowse draws inside the page: ember focus brackets around a dot, with a fastbrowse tag. It glides
// along a shallow arc to each action point, the brackets close in as it lands, and each press sends out a ring. A drag
// carries it along the drag's own path.
//
// It runs in an isolated world, so page scripts cannot read its state or call it, and draws into a closed shadow root
// on one host element, so page code that walks the DOM finds only an empty element. The host takes no input
// (`pointer-events: none`), sits in the top layer as a manual popover so it shows above dialogs, popovers and a
// fullscreen element, and is put back if the page removes it. snapshot.js and capture.js skip the host by its
// attribute, and the run hides it while it takes a screenshot for the model.
//
// Evaluated as an expression: the first evaluation in a document installs the controller, later ones return it.
(() => {
  if (globalThis.__fastbrowseCursor) return globalThis.__fastbrowseCursor;

  const HOST_ATTRIBUTE = 'data-fastbrowse-cursor';
  const EMBER = '#c55230';
  const INK = '#17150f';
  const LOCK_MS = 160; // the brackets closing in as the cursor lands
  const PULSE_MS = 400; // a press's ring swelling and fading
  const PRESSED_MS = 130; // how long the marker stays shrunk after a press
  const OPEN = 34; // the brackets' half-width as the cursor lands
  const CLOSED = 18; // and at rest
  const ARM = 8; // the length of a bracket's arm
  const ARC = 0.2; // how far a glide bows to one side, as a fraction of its length
  const SVG = 'http://www.w3.org/2000/svg';

  const host = document.createElement('div');
  host.setAttribute(HOST_ATTRIBUTE, '');
  host.setAttribute('aria-hidden', 'true');
  host.setAttribute('popover', 'manual');
  // Inline, so neither the page's styles nor the popover defaults can move, size or paint the host.
  host.style.cssText = [
    'all: initial', 'position: fixed', 'inset: 0', 'width: 100vw', 'height: 100vh', 'margin: 0', 'padding: 0',
    'border: 0', 'background: transparent', 'overflow: visible', 'pointer-events: none', 'z-index: 2147483647',
    'display: block', 'contain: strict',
  ].map((rule) => `${rule} !important`).join(';');
  const root = host.attachShadow({ mode: 'closed' });
  const svg = document.createElementNS(SVG, 'svg');
  svg.setAttribute('width', '100%');
  svg.setAttribute('height', '100%');
  svg.style.cssText = 'position:absolute;inset:0;overflow:visible;pointer-events:none';
  root.appendChild(svg);

  const state = {
    at: null, // [x, y] in viewport CSS pixels, null until the first move
    glide: null, // {from, bend, to, start, ms}
    trace: null, // {points, start, ms}: a drag's path
    landed: -Infinity,
    pressedUntil: -Infinity,
    pulses: [], // [{at, start}]
    hidden: false,
    frame: 0,
  };

  const ease = (u) => (u < 0.5 ? 4 * u * u * u : 1 - (-2 * u + 2) ** 3 / 2);
  const clamp = (u) => Math.max(0, Math.min(1, u));
  const node = (tag, attributes, parent) => {
    const element = document.createElementNS(SVG, tag);
    for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
    parent.appendChild(element);
    return element;
  };

  // Where the cursor is at `now`, and whether it is still travelling.
  function position(now) {
    const { glide, trace } = state;
    if (trace) {
      const { points, start, ms } = trace;
      const u = clamp((now - start) / ms);
      if (u >= 1 || points.length < 2) {
        state.trace = null;
        state.at = points[points.length - 1];
        return [state.at, false];
      }
      // Even pace along the path: the input dispatch sends its moves at a steady rate too.
      const scaled = u * (points.length - 1);
      const i = Math.floor(scaled);
      const [ax, ay] = points[i];
      const [bx, by] = points[i + 1];
      const f = scaled - i;
      return [[ax + (bx - ax) * f, ay + (by - ay) * f], true];
    }
    if (glide) {
      const u = ease(clamp((now - glide.start) / glide.ms));
      const { from: [ax, ay], bend: [cx, cy], to: [bx, by] } = glide;
      if (u >= 1) {
        state.glide = null;
        state.at = glide.to;
        state.landed = glide.start + glide.ms;
        return [state.at, false];
      }
      const v = 1 - u;
      return [[v * v * ax + 2 * v * u * cx + u * u * bx, v * v * ay + 2 * v * u * cy + u * u * by], true];
    }
    return [state.at, false];
  }

  function brackets(group, half) {
    for (const [sx, sy] of [[-1, -1], [1, -1], [1, 1], [-1, 1]]) {
      const x = sx * half;
      const y = sy * half;
      const d = `M ${x} ${y - sy * ARM} L ${x} ${y} L ${x - sx * ARM} ${y}`;
      const line = { d, fill: 'none', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' };
      node('path', { ...line, stroke: '#fff', 'stroke-width': 5.5 }, group);
      node('path', { ...line, stroke: EMBER, 'stroke-width': 2.6 }, group);
    }
  }

  function tag(group) {
    const pill = node('g', { transform: 'translate(26 12)' }, group);
    node('rect', { width: 92, height: 22, rx: 11, fill: INK }, pill);
    node('circle', { cx: 12, cy: 11, r: 4, fill: EMBER }, pill);
    const label = node('text', {
      x: 21, y: 15.5, fill: '#fff', 'font-size': 11.5, 'font-weight': 600,
      'font-family': 'Inter, "Helvetica Neue", Arial, sans-serif',
    }, pill);
    label.textContent = 'fastbrowse';
  }

  function draw(now) {
    state.frame = 0;
    svg.replaceChildren();
    const [at, moving] = position(now);
    for (const pulse of state.pulses) {
      const u = (now - pulse.start) / PULSE_MS;
      if (u < 0 || u >= 1) continue;
      node('circle', {
        cx: pulse.at[0], cy: pulse.at[1], r: 8 + 30 * ease(u), fill: 'none', stroke: EMBER, 'stroke-width': 2.5,
        opacity: 0.8 * (1 - u),
      }, svg);
    }
    state.pulses = state.pulses.filter((pulse) => now - pulse.start < PULSE_MS);
    if (at) {
      const group = node('g', { transform: `translate(${at[0]} ${at[1]})` }, svg);
      const sinceLanding = now - state.landed;
      const half = moving || sinceLanding >= LOCK_MS ? CLOSED : OPEN - (OPEN - CLOSED) * ease(clamp(sinceLanding / LOCK_MS));
      brackets(group, half);
      const pressed = state.trace || now < state.pressedUntil;
      const marker = node('g', { transform: `scale(${pressed ? 0.85 : 1})` }, group);
      node('circle', { r: 7.5, cy: 1.5, fill: 'rgba(0,0,0,.25)' }, marker);
      node('circle', { r: 7, fill: '#fff' }, marker);
      node('circle', { r: 4.6, fill: EMBER }, marker);
      tag(group);
    }
    const animating = moving || now - state.landed < LOCK_MS || now < state.pressedUntil || state.pulses.length;
    if (animating) schedule();
  }

  function schedule() {
    if (!state.frame) state.frame = requestAnimationFrame(draw);
  }

  // The host goes back to the top of the top layer: a dialog or fullscreen element opened since would cover it.
  function raise() {
    if (state.hidden) return;
    if (!host.isConnected) (document.body || document.documentElement).appendChild(host);
    try {
      if (host.matches(':popover-open')) host.hidePopover();
      host.showPopover();
    } catch {
      // A document that forbids popovers still gets the fixed host at the highest z-index.
    }
  }

  new MutationObserver(() => {
    if (!host.isConnected && !state.hidden) raise();
  }).observe(document, { childList: true, subtree: true });
  document.addEventListener('fullscreenchange', raise, true);

  // Resolves once the cursor has landed and the brackets have closed. Timed by the clock rather than by drawn frames,
  // so a tab that is not painting never holds up the action.
  const after = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  // Each drawing call clears `hidden` itself, so a hide never followed by its show (a run cancelled mid-capture) does
  // not keep the cursor out of the page for the rest of the document.
  const api = {
    // Glide to (x, y) over `ms`, starting from `from` when this document has not drawn the cursor yet.
    glide(x, y, ms, from) {
      state.hidden = false;
      raise();
      const now = performance.now();
      const [start] = position(now);
      const origin = start || from || null;
      const to = [x, y];
      state.trace = null;
      if (!origin || ms <= 0) {
        state.glide = null;
        state.at = to;
        state.landed = now;
      } else {
        const [ax, ay] = origin;
        const dx = x - ax;
        const dy = y - ay;
        const bend = [(ax + x) / 2 - dy * ARC, (ay + y) / 2 + dx * ARC];
        state.glide = { from: origin, bend, to, start: now, ms };
      }
      schedule();
      return after((state.glide ? ms : 0) + LOCK_MS);
    },
    // A press at the cursor's point: a ring goes out and the marker shrinks for a moment.
    press(x, y) {
      state.hidden = false;
      raise();
      const now = performance.now();
      state.glide = null;
      state.at = [x, y];
      state.pulses.push({ at: [x, y], start: now });
      state.pressedUntil = now + PRESSED_MS;
      schedule();
    },
    // Follow a drag: press at the first point, travel the rest over `ms`, release at the last.
    trace(points, ms) {
      if (!points.length) return;
      state.hidden = false;
      raise();
      const now = performance.now();
      state.glide = null;
      state.at = points[0];
      state.pulses.push({ at: points[0], start: now });
      state.trace = { points, start: now, ms: Math.max(ms, 1) };
      schedule();
    },
    // Taken out of the page while a screenshot for the model is captured, then put back where it was.
    hide() {
      state.hidden = true;
      host.remove();
      // The next frame no longer has it; a tab that paints nothing has no frame to wait for.
      return Promise.race([new Promise((resolve) => requestAnimationFrame(() => resolve())), after(100)]).then(() => true);
    },
    show() {
      state.hidden = false;
      raise();
      schedule();
    },
  };
  globalThis.__fastbrowseCursor = api;
  return api;
})()
