// The agent cursor fastbrowse draws inside the page: ember focus brackets around a dot. It glides along a shallow arc
// to each action point with the brackets at their resting size; every press (a click, a fill's click, a drag's press)
// moves the brackets in towards the dot and back, and leaves a faint disc swelling and fading where it was pressed. A
// drag carries it along the drag's own path. The look and timing are deskpilot's agent cursor (`drawOne` and
// `glideTo` in src/deskpilot/plugin/deskpilot.cpp), in CSS pixels, with fastbrowse's colour and centre dot.
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
  // A disconnected controller can leave its host in the attached document.
  for (const stale of document.querySelectorAll(`[${HOST_ATTRIBUTE}]`)) {
    stale.removeAttribute(HOST_ATTRIBUTE);
    stale.remove();
  }
  const EMBER = '#c55230';
  const ARM = 6; // a bracket's arm, end to end, and its rounded stroke
  const STROKE = 1.8;
  const REST = 15; // the brackets' half-width at rest and while gliding
  const PRESS = 7; // and at the deepest point of a press
  // A press: the brackets move in over SQUEEZE_IN ms (cubic ease-out), stay for SQUEEZE_HOLD and go back over
  // SQUEEZE_OUT (cubic ease-in-out), while the dot shrinks to PRESSED of its size.
  const SQUEEZE_IN = 50;
  const SQUEEZE_HOLD = 70;
  const SQUEEZE_OUT = 100;
  const SQUEEZE = SQUEEZE_IN + SQUEEZE_HOLD + SQUEEZE_OUT;
  const PRESSED = 0.85;
  // A press's disc: filled at 0.3 opacity fading out, growing from 0.3 to 1 times PULSE_SIZE (cubic ease-out).
  const PULSE_MS = 380;
  const PULSE_SIZE = 30;
  const DOT = 2.4; // fastbrowse's own centre: a dot of this radius
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
    glide: null, // {from, bend, to, start, ms}; `start` may be ahead, after the last press's squeeze
    trace: null, // {points, start, ms}: a drag's path
    pressed: -Infinity, // when the last press was sent
    pulses: [], // [{at, start}]
    hidden: false,
    frame: 0,
  };

  const easeInOut = (u) => (u < 0.5 ? 4 * u * u * u : 1 - (-2 * u + 2) ** 3 / 2);
  const easeOut = (u) => 1 - (1 - u) ** 3;
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
      const u = easeInOut(clamp((now - glide.start) / glide.ms));
      const { from: [ax, ay], bend: [cx, cy], to: [bx, by] } = glide;
      if (u >= 1) {
        state.glide = null;
        state.at = glide.to;
        return [state.at, false];
      }
      const v = 1 - u;
      return [[v * v * ax + 2 * v * u * cx + u * u * bx, v * v * ay + 2 * v * u * cy + u * u * by], true];
    }
    return [state.at, false];
  }

  // How far in the brackets are `elapsed` ms after a press: 0 at rest, 1 at PRESS.
  function inward(elapsed) {
    if (elapsed < 0 || elapsed >= SQUEEZE) return 0;
    if (elapsed < SQUEEZE_IN) return easeOut(elapsed / SQUEEZE_IN);
    return 1 - easeInOut(Math.max(0, elapsed - SQUEEZE_IN - SQUEEZE_HOLD) / SQUEEZE_OUT);
  }

  // Four corners `half` from the centre, each arm running inwards from its corner. The arm's path is its length less
  // the stroke, so with round caps it spans ARM from the corner's outer edge.
  function brackets(group, half) {
    const reach = ARM - STROKE;
    for (const [sx, sy] of [[-1, -1], [1, -1], [1, 1], [-1, 1]]) {
      const x = sx * half;
      const y = sy * half;
      node('path', {
        d: `M ${x} ${y - sy * reach} L ${x} ${y} L ${x - sx * reach} ${y}`,
        fill: 'none', stroke: EMBER, 'stroke-width': STROKE, 'stroke-linecap': 'round', 'stroke-linejoin': 'round',
      }, group);
    }
  }

  function draw(now) {
    state.frame = 0;
    svg.replaceChildren();
    const [at, moving] = position(now);
    for (const pulse of state.pulses) {
      const u = (now - pulse.start) / PULSE_MS;
      if (u < 0 || u >= 1) continue;
      node('circle', {
        cx: pulse.at[0], cy: pulse.at[1], r: PULSE_SIZE * (0.3 + 0.7 * easeOut(u)), fill: EMBER, opacity: 0.3 * (1 - u),
      }, svg);
    }
    state.pulses = state.pulses.filter((pulse) => now - pulse.start < PULSE_MS);
    const squeezing = now - state.pressed < SQUEEZE;
    if (at) {
      const group = node('g', { transform: `translate(${at[0]} ${at[1]})` }, svg);
      brackets(group, REST + (PRESS - REST) * inward(now - state.pressed));
      node('circle', { r: DOT * (squeezing ? PRESSED : 1), fill: EMBER }, group);
    }
    if (moving || squeezing || state.pulses.length) schedule();
  }

  function schedule() {
    if (!state.frame) state.frame = requestAnimationFrame(draw);
  }

  // The host goes back to the top of the top layer: a dialog or fullscreen element opened since would cover it.
  function raise() {
    if (state.hidden || !host.hasAttribute(HOST_ATTRIBUTE)) return;
    if (!host.isConnected) (document.body || document.documentElement).appendChild(host);
    try {
      if (host.matches(':popover-open')) host.hidePopover();
      host.showPopover();
    } catch {
      // A document that forbids popovers still gets the fixed host at the highest z-index.
    }
  }

  const observer = new MutationObserver(() => {
    if (!host.isConnected && !state.hidden) raise();
  });
  observer.observe(document, { childList: true, subtree: true });
  document.addEventListener('fullscreenchange', raise, true);

  // Resolves once the cursor has landed. Timed by the clock rather than by drawn frames, so a tab that is not painting
  // never holds up the action.
  const after = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  // A press at `at`: the brackets move in and back and a disc swells there.
  function pressAt(at, now) {
    state.pressed = now;
    state.pulses.push({ at, start: now });
  }

  // Each drawing call clears `hidden` itself, so a hide never followed by its show (a run cancelled mid-capture) does
  // not keep the cursor out of the page for the rest of the document.
  const api = {
    // Glide to (x, y) over `ms`, starting from `from` when this document has not drawn the cursor yet. A press still
    // squeezing keeps its whole squeeze: the glide starts once it is over.
    glide(x, y, ms, from) {
      state.hidden = false;
      raise();
      const now = performance.now();
      const [start] = position(now);
      const origin = start || from || null;
      const to = [x, y];
      state.trace = null;
      let wait = 0;
      if (!origin || ms <= 0) {
        state.glide = null;
        state.at = to;
      } else {
        const [ax, ay] = origin;
        const dx = x - ax;
        const dy = y - ay;
        const bend = [(ax + x) / 2 - dy * ARC, (ay + y) / 2 + dx * ARC];
        const begin = Math.max(now, state.pressed + SQUEEZE);
        state.at = origin;
        state.glide = { from: origin, bend, to, start: begin, ms };
        wait = begin - now + ms;
      }
      schedule();
      return after(wait);
    },
    // A press at (x, y), sent as the input's press is.
    press(x, y) {
      state.hidden = false;
      raise();
      const now = performance.now();
      state.glide = null;
      state.trace = null;
      state.at = [x, y];
      pressAt([x, y], now);
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
      pressAt(points[0], now);
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
    dispose() {
      observer.disconnect();
      document.removeEventListener('fullscreenchange', raise, true);
      cancelAnimationFrame(state.frame);
      host.remove();
      delete globalThis.__fastbrowseCursor;
    },
    show() {
      if (!state.at) return;
      state.hidden = false;
      raise();
      schedule();
    },
  };
  globalThis.__fastbrowseCursor = api;
  return api;
})()
