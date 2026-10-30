//! The scripts `CdpPage` evaluates in the page, with the comments that explain them. A script built from arguments
//! is built in `page` by a function of the same name.

use std::sync::LazyLock;

use serde_json::Value;

use crate::session::py_json;

/// Longest wait for the scroll event a scrolled window owes before the control is left untouched as unsettled.
pub const SCROLL_EVENT_SECONDS: f64 = 3.0;
/// A deadline, not a wait: a field with no editor to open settles on the first frame.
pub const HANDOFF_SECONDS: f64 = 0.6;
pub const HANDOFF_QUIET_SECONDS: f64 = 0.1;

/// What a page draws over its content and means to be dealt with first. Naming a cover for recovery and routing
/// around one read this one list: a small open menu fixed to the window looked like a band to one of them, and the
/// control under it would have been pressed through it.
pub const OBSTRUCTIONS: &str =
    "dialog,[role=dialog],[role=alertdialog],[aria-modal=\"true\"],[role=menu],[role=listbox],[popover]";

/// A float as Python's `str` writes it into an f-string: `3000.0`, not `3000`.
pub fn py_float(value: f64) -> String {
    let text = format!("{value}");
    if text.contains(['.', 'e', 'i', 'N']) { text } else { format!("{text}.0") }
}

/// The element a control names as its popup, or null.
pub const OWNED_JS: &str = "(e => { const id = e.getAttribute('aria-controls') || e.getAttribute('aria-owns'); \
     return id ? e.ownerDocument.getElementById(id) : null; })";

/// Whether a field asks for suggestions.
pub static SUGGESTS_JS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "(e => e.getAttribute('role') === 'combobox' || e.type === 'search' || \
         !!e.getAttribute('aria-autocomplete') || !!{OWNED_JS}(e))"
    )
});

pub const SELECT_TEXT_JS: &str = concat!(
    "if (typeof e.select === 'function') e.select(); else { const range = e.ownerDocument.createRange(); ",
    "range.selectNodeContents(e); const selection = e.ownerDocument.getSelection(); ",
    "selection.removeAllRanges(); selection.addRange(range); } ",
);

pub const HIT_TEST_JS: &str = concat!(
    "((id, scroll = true) => { const e = window.__fastbrowse?.nodes.get(id); ",
    "if (!e?.isConnected || e.matches(':disabled') || e.closest('[aria-disabled=\"true\"],[inert]') || ",
    "!window.__fastbrowse.visible(e)) return null; ",
    // Scrolling only when the control is not already in full view: a page scroll closes open menus and
    // popups, so centring an option that was already visible dismissed its menu before the click landed.
    "const w = e.ownerDocument.defaultView, v = e.getBoundingClientRect(); ",
    "if (scroll && (v.top < 0 || v.left < 0 || v.bottom > w.innerHeight || v.right > w.innerWidth)) ",
    "e.scrollIntoView({block: 'center', inline: 'nearest', behavior: 'instant'}); ",
    "const labels = e.tagName === 'INPUT' && ['checkbox', 'radio'].includes(e.type) ? [...e.labels] : []; ",
    // A control's centre can be covered while its edges remain clickable. Bound the search on wrapped
    // controls, and check each point through its frames so a parent overlay still prevents dispatch. A checkbox
    // or radio is also pressed through its own labels, as a person presses it: a swatch's input is often a
    // visually hidden 1px box whose only point lies on its wrapper, while its label is the swatch drawn beside it.
    "const box = r => ({left: Math.max(0, r.left), top: Math.max(0, r.top), ",
    "right: Math.min(w.innerWidth, r.right), bottom: Math.min(w.innerHeight, r.bottom)}); ",
    "const drawn = r => r.right > r.left && r.bottom > r.top; ",
    "const rects = [...[...e.getClientRects()].map(box).filter(drawn).slice(0, 4), ",
    "...labels.flatMap(l => [...l.getClientRects()].map(box).filter(drawn).slice(0, 4))]; ",
    "if (!rects.length) return null; ",
    "const ACTIVE = 'a[href],button,input,select,textarea,summary,[contenteditable]:not([contenteditable=false]),",
    "[role=button],[role=link],[role=checkbox],[role=radio],[role=switch],[role=tab],[role=option],",
    "[role=menuitem],[role=menuitemcheckbox],[role=menuitemradio],[role=combobox],[role=textbox],",
    "[role=searchbox],[role=slider],[role=spinbutton],[role=treeitem],[role=gridcell]'; ",
    "const hitAt = (x, y) => { ",
    // Descend through open shadow roots: the document-level hit is only the outermost host.
    "let node = e, doc = e.ownerDocument; while (true) { ",
    "const view = doc.defaultView; ",
    "if (x < 0 || y < 0 || x >= view.innerWidth || y >= view.innerHeight) return null; ",
    "let hit = doc.elementFromPoint(x, y); ",
    "while (hit?.shadowRoot) { const inner = hit.shadowRoot.elementFromPoint(x, y); ",
    "if (!inner || inner === hit) break; hit = inner; } ",
    // A control nested inside the target (a row's Delete button, a link or checkbox inside a label) takes the
    // click itself, so a point is the target's only when no other control sits between it and the hit: one the
    // snapshot offered, a widget, or a label with its own input. A label and its input are one target. A hover
    // target is not one: it reveals content but takes no click from its container.
    "const same = x => x === e || x.control === e || e.control === x; ",
    "const owns = root => { if (!root.contains(hit)) return false; ",
    "for (let x = hit; x !== root; x = x.parentElement) { if (same(x)) return true; ",
    "if (x.matches(ACTIVE) || window.__fastbrowse.controls.has(x) || x.control) return false; } return true; }; ",
    "if (node === e ? !owns(e) && !labels.some(owns) : !node.contains(hit)) return 'covered'; ",
    "if (doc === document) break; ",
    "node = view.frameElement; if (!node) return null; ",
    "const frame = node.getBoundingClientRect(); ",
    "x = frame.x + (node.clientLeft + x) * frame.width / node.offsetWidth; ",
    "y = frame.y + (node.clientTop + y) * frame.height / node.offsetHeight; doc = node.ownerDocument; } ",
    "return [x, y]; }; ",
    "for (const r of rects) { for (const [fx, fy] of [[.5, .5], [.25, .25], [.75, .25], [.25, .75], [.75, .75]]) { ",
    "const point = hitAt(r.left + (r.right - r.left) * fx, r.top + (r.bottom - r.top) * fy); ",
    "if (Array.isArray(point)) return point; } } return 'covered'; })",
);

/// The pointer path rechecks a control's guard before every press (`stable_point`); the keyboard path asks this.
pub const GUARD_HOLDS_JS: &str = concat!(
    "((id, guard) => { const r = window.__fastbrowse, e = r?.nodes.get(id); ",
    "return !!e?.isConnected && !!r.guardKey && r.guardKey(e) === guard; })",
);

/// Keys go to the focused control whatever is drawn over it, so a control a band still covers is pressed from the
/// keyboard, as a keyboard user would press it, and nothing in the band is ever touched. The key is the one the
/// control's own semantics answer to; a control that names none is not pressed.
pub const ACTIVATION_KEY_JS: &str = concat!(
    "((id) => { const e = window.__fastbrowse?.nodes.get(id); if (!e?.isConnected) return null; ",
    "const role = e.getAttribute('role'); ",
    "if (e.tagName === 'INPUT' && ['checkbox', 'radio'].includes(e.type)) return ' '; ",
    "if (['checkbox', 'radio', 'switch', 'menuitemcheckbox', 'menuitemradio'].includes(role)) return ' '; ",
    "if (e.matches('button,a[href],summary,input[type=submit],input[type=button],input[type=reset],",
    "input[type=image]')) return 'Enter'; ",
    "if (['button', 'link', 'tab', 'menuitem', 'option', 'treeitem'].includes(role)) return 'Enter'; ",
    "return null; })",
);

pub const GEOMETRY_JS: &str = concat!(
    "[window.screenX, window.screenY, window.outerWidth, window.outerHeight, window.innerWidth, window.innerHeight, ",
    "window.visualViewport ? window.visualViewport.scale : 1, document.visibilityState === 'visible', ",
    "String(performance.timeOrigin)]",
);

/// Resolves once the page has drawn a frame, or after 100ms where a hidden page never draws one. A guard resumed
/// inside an animation callback can miss other callbacks in that same frame, so it resolves in the next task, where
/// a combined frame wait and hit test sees their DOM changes too.
pub const PRESENTED_JS: &str = concat!(
    "new Promise(done => { const r = window.__fastbrowse; const t = r.nativeSetTimeout(done, 100); ",
    "r.nativeRequestAnimationFrame(() => r.nativeRequestAnimationFrame(() => ",
    "r.nativeSetTimeout(() => { r.nativeClearTimeout(t); done(); }, 0))); })",
);

/// A band fixed to the window's foot covered a form's submit button, and the run ended stuck after clicking the
/// band's own close control did nothing. A band fixed to the window covers a place on the screen, not a control, so
/// the control is scrolled to each alignment in turn and kept at the first where a point of it is its own again. The
/// guard is checked in the same task: a stale control must never be scrolled into view.
///
/// It resolves only once every scroll it caused has been delivered and the page has gone a frame and a task without
/// another, when any alignment moved the window or a container the control sits in: the page's scroll handlers,
/// which can repurpose the control, have run by then. A container's scroll event does not reach the window, so a
/// route that waited on the window alone, with only a scrolled list moving, went ahead before the list's handler
/// ran. One capture listener on each document sees every element's scroll and the window's. A wait for a drawn
/// frame gave up after 100ms without one, and comparing only where the window started and ended skipped the wait
/// when 'end' brought it back to the top. A wait with any deadline can lose the race on a loaded machine, so a
/// scroll whose events did not arrive and go quiet in time, or that a hidden page will only deliver once something
/// shows it, is reported 'unsettled' and nothing is acted on.
pub static UNCOVER_JS: LazyLock<String> = LazyLock::new(|| {
    [
    "(async (id, guard) => { const r = window.__fastbrowse, e = r?.nodes.get(id); ",
    "if (!e?.isConnected || r.guardKey(e) !== guard) return null; ",
    "const docs = [], scrollers = [], roots = []; ",
    // The scrolling element reports the window's scroll, whose events and scrollend go to the document instead.
    "for (let n = e; n; ) { if (n !== n.ownerDocument.scrollingElement) scrollers.push(n); ",
    "const root = n.getRootNode(); if (root instanceof ShadowRoot && !roots.includes(root)) roots.push(root); ",
    "if (n.parentElement) n = n.parentElement; else if (root instanceof ShadowRoot) n = root.host; ",
    "else { const view = n.ownerDocument.defaultView; docs.push(n.ownerDocument); ",
    "try { n = view.frameElement; } catch { n = null; } } } ",
    "const at = t => t instanceof Document ? [t.defaultView.scrollX, t.defaultView.scrollY].join() ",
    ": [t.scrollLeft, t.scrollTop].join(); const targets = [...docs, ...scrollers]; ",
    "const where = () => targets.map(at).join(';'); const before = targets.map(at); ",
    // A scroll event is not composed, so one inside a shadow root, open or closed, never reaches the document: a
    // list scrolled there was waited on until the deadline and refused a control it had exposed. Every document,
    // every shadow root on the way and every scroller listens, and an event heard twice counts once.
    "const heard = new WeakSet(), ends = new Set(); let seen = 0; ",
    "const mark = ev => { if (!heard.has(ev)) { heard.add(ev); seen++; } }; const end = ev => ends.add(ev.target); ",
    "const listeners = [...docs, ...roots, ...scrollers]; ",
    "for (const l of listeners) { l.addEventListener('scroll', mark, {capture: true, passive: true}); ",
    "l.addEventListener('scrollend', end, {capture: true, passive: true}); } ",
    "try { let moved = false, point = 'covered'; ",
    "for (const block of ['center', 'start', 'end']) { const was = where(); ",
    "e.scrollIntoView({block, inline: 'nearest', behavior: 'instant'}); moved ||= where() !== was; ",
    "point = (",
    HIT_TEST_JS,
    ")(id, false); if (point !== 'covered') break; } ",
    "if (!moved) return point; if (e.ownerDocument.hidden) return 'unsettled'; ",
    // A scroller that ends somewhere new owes its scrollend too, where the browser sends one: a carousel that
    // settles on its scrollend swaps the card, and the button in it, after the scroll events have gone quiet.
    "const due = 'onscrollend' in window ? targets.filter((t, i) => at(t) !== before[i]) : []; ",
    &format!("const deadline = performance.now() + {}; ", py_float(SCROLL_EVENT_SECONDS * 1000.0)),
    "const turn = () => Promise.race([",
    "new Promise(done => requestAnimationFrame(() => setTimeout(() => done(true), 0))), ",
    "new Promise(done => setTimeout(() => done(false), Math.max(0, deadline - performance.now())))]); ",
    "while (!seen || due.some(t => !ends.has(t))) if (!await turn()) return 'unsettled'; ",
    "for (let last = -1; last !== seen; ) { last = seen; if (!await turn()) return 'unsettled'; } ",
    "return point; } finally { for (const l of listeners) { l.removeEventListener('scroll', mark, {capture: true}); ",
    "l.removeEventListener('scrollend', end, {capture: true}); } } })",
    ]
    .concat()
});

/// What is drawn at a point, through open shadow roots, and walks up through shadow hosts: an overlay drawn by a custom
/// element keeps its fixed box, or its dialog role, inside its shadow root, where a walk by parentElement stops.
pub const DEEP_HIT_JS: &str = "const deepHit = (doc, x, y) => { let hit = doc.elementFromPoint(x, y); \
     while (hit?.shadowRoot) { const inner = hit.shadowRoot.elementFromPoint(x, y); \
     if (!inner || inner === hit) break; hit = inner; } return hit; }; \
     const up = n => n.parentElement || (n.getRootNode() instanceof ShadowRoot ? n.getRootNode().host : null); \
     const holds = (a, b) => { for (let n = b; n; n = up(n)) if (n === a) return true; return false; }; ";

/// What covers the control's centre, through its frames: an obstruction the page means to be dealt with first (a
/// dialog, an open menu, a popover), a band fixed to the window (an anchored ad, a sticky footer, a chat bubble)
/// that floats over whatever is scrolled beneath it, or anything else, such as a full-window backdrop or a panel
/// drawn over the control in the page's own flow. Only a band is routed around; under anything else the page shows
/// something other than the control, and that is the run's to deal with.
pub static COVER_JS: LazyLock<String> = LazyLock::new(|| {
    [
        "((id) => { const e = window.__fastbrowse?.nodes.get(id); if (!e?.isConnected) return null; ",
        DEEP_HIT_JS,
        "const OBSTRUCTIONS = ",
        &py_json(&Value::from(OBSTRUCTIONS)),
        "; ",
        "const rect = e.getBoundingClientRect(); let x = rect.left + rect.width / 2, y = rect.top + rect.height / 2; ",
        "let node = e, doc = e.ownerDocument; while (true) { const view = doc.defaultView; ",
        "const hit = deepHit(doc, x, y); ",
        "if (hit && !holds(node, hit)) { let obstruction = null; ",
        "for (let n = hit; n && !obstruction; n = up(n)) if (n.matches(OBSTRUCTIONS)) obstruction = n; ",
        "if (obstruction && !holds(obstruction, node)) return 'obstruction'; ",
        "let band = false; for (let n = hit; n && !holds(n, node); n = up(n)) { ",
        "if (!['fixed', 'sticky'].includes(view.getComputedStyle(n).position)) continue; ",
        "const r = n.getBoundingClientRect(); ",
        "if (r.width * r.height >= view.innerWidth * view.innerHeight / 2) return 'other'; band = true; } ",
        "return band ? 'band' : 'other'; } ",
        "if (doc === document) return 'none'; node = view.frameElement; if (!node) return 'none'; ",
        "const frame = node.getBoundingClientRect(); ",
        "x = frame.x + (node.clientLeft + x) * frame.width / node.offsetWidth; ",
        "y = frame.y + (node.clientTop + y) * frame.height / node.offsetHeight; doc = node.ownerDocument; } })",
    ]
    .concat()
});

/// A field that opens an editor over itself when clicked (a search overlay, an airport picker) moves focus to that
/// editor; typing into the original, now hidden behind it, reaches no suggestion list. A person types where focus
/// went, so the fill follows focus to an editable field in the same document that covers the spot saved before the
/// click, even if the original was detached. The editor can take focus a frame or a timer after the click, so the
/// check waits for the click's DOM changes to go quiet, briefly, first.
pub static HANDED_FOCUS_JS: LazyLock<String> = LazyLock::new(|| {
    [
    "((id, secret) => new Promise(resolve => { const r = window.__fastbrowse, e = r?.nodes.get(id), was = r?.handoff; ",
    "if (!e || !was) { resolve(null); return; } ",
    "const decide = () => { ",
    "if (e.ownerDocument !== was.doc || was.doc.defaultView?.document !== was.doc) return null; ",
    "const a = was.root.activeElement; ",
    "if (!a || a === e || e.contains(a) || secret) return e.isConnected ? id : null; ",
    "const text = a.isContentEditable || a.tagName === 'TEXTAREA' || (a.tagName === 'INPUT' && ",
    "!['button', 'checkbox', 'color', 'file', 'hidden', 'image', 'radio', 'range', 'reset', 'submit']",
    ".includes(a.type)); if (!text || !a.isConnected || a.disabled || a.readOnly || a.ownerDocument !== was.doc || ",
    "a.closest('[aria-disabled=\"true\"],[aria-readonly=\"true\"],[inert]') || ",
    "!a.checkVisibility({checkOpacity: true, checkVisibilityCSS: true})) return e.isConnected ? id : null; ",
    "const now = a.getBoundingClientRect(); ",
    "if (was.x < now.left || was.x > now.right || was.y < now.top || was.y > now.bottom) ",
    "return e.isConnected ? id : null; ",
    "if (!r.ids.has(a)) r.ids.set(a, r.next++); const n = r.ids.get(a); r.nodes.set(n, a); return n; }; ",
    &format!("const deadline = performance.now() + {}; ", py_float(HANDOFF_SECONDS * 1000.0)),
    "const poll = () => { const choice = decide(); ",
    "if ((choice !== null && performance.now() - (r.lastMutation ?? 0) >= ",
    &format!("{}) || performance.now() >= deadline) resolve(choice); ", py_float(HANDOFF_QUIET_SECONDS * 1000.0)),
    "else setTimeout(poll, 20); }; ",
    // Chrome can hold a cross-origin frame's animation frames indefinitely, and CI hung in pytest for an hour
    // awaiting one; the timer starts the poll anyway once the deadline has passed.
    "let started = false; const start = () => { if (!started) { started = true; poll(); } }; ",
    &format!("setTimeout(start, {}); ", py_float(HANDOFF_SECONDS * 1000.0)),
    "if (was.doc.hidden) start(); else requestAnimationFrame(() => setTimeout(start, 0)); }))",
    ]
    .concat()
});
