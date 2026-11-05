# A run drives the user's own Chrome through an extension that relays CDP over chrome.debugger

Chrome asks "Allow remote debugging?" on every websocket connection to a browser opened at chrome://inspect, and
never remembers the answer (#630). A fastbrowse extension that relays the CDP commands we already send through
`chrome.debugger` raises no dialog, keeps the user's sign-ins, and reuses the Rust CDP stack: the tab session works
as is, and only the refused browser-level Target calls move to `chrome.tabs` and `chrome.debugger.attach`. Claude in
Chrome, Codex and Playwright's `--extension` mode take the same route.

The cost is Chrome's "started debugging this browser" infobar while a run is attached. We accept it: there is nothing
to click, it closes 5 s after the run detaches, and pressing Cancel ends the run cleanly. An opt-in policy install
removes it. Acting through content scripts and `chrome.scripting` instead was rejected: it would be a second
implementation of every action, and its synthetic events are not trusted input. Evidence:
`docs/research/extension-prior-art.md`, `extension-cdp-coverage.md`, `extension-chrome-rules.md`.
