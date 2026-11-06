# CDP coverage under a Chrome MV3 extension backend

Research for #786, feeding the extension backend in #630. It lists every CDP method and event fastbrowse uses, says whether `chrome.debugger` allows it from an extension, which extension API can stand in, and what breaks a run.

Code read: `origin/main` at `44d89533` and `origin/cb/recipes`. Every method used on `main` is also used on `cb/recipes`. Methods only on `cb/recipes` are marked **recipes**. Only `crates/fastbrowse-browser` sends CDP. No other crate does, and no code calls the `Accessibility`, `DOMSnapshot`, `Browser.setDownloadBehavior` or `Page.printToPDF` methods. The element tree, hit tests and geometry all run as page JavaScript through `Runtime.evaluate` (`page/scripts.rs`, `js/`).

## How chrome.debugger differs from a DevTools socket

Sources: the [API reference](https://developer.chrome.com/docs/extensions/reference/api/debugger) and Chromium `main` files [`chrome/browser/extensions/api/debugger/debugger_api.cc`](https://source.chromium.org/chromium/chromium/src/+/main:chrome/browser/extensions/api/debugger/debugger_api.cc) (`debugger_api.cc` below), `content/browser/devtools/render_frame_devtools_agent_host.cc` (`rfdah.cc`), `content/browser/devtools/protocol/target_handler.cc` (`target_handler.cc`), and `page_handler.cc`, `browser_handler.cc` and `system_info_handler.cc` in the same folder.

1. **No browser target.** An extension attaches to `{tabId}`, `{targetId}` or `{extensionId}`. Only the Perfetto UI extension may attach to the `"browser"` target (`ExtensionIsTrusted`, `debugger_api.cc`). So every command runs on a tab's root session or on one of its child sessions. fastbrowse sends its `Target.*`, `Browser.getVersion` and `SystemInfo.*` calls with no session (`session_id: None`), which means browser level, and none of those can stay as they are.
2. **The Target domain is in auto-attach-only mode.** `rfdah.cc` creates the tab's `TargetHandler` with `AccessMode::kAutoAttachOnly` unless the client is trusted. In that mode `target_handler.cc` answers "Not allowed" to `setDiscoverTargets`, `attachToTarget`, `createTarget`, `activateTarget` and `getTargets`. `getTargetInfo` works only for the session's own target, and `closeTarget` works only for the own target or an auto-attached one. `setAutoAttach` (with `flatten`) and `detachFromTarget` work.
3. **Flat child sessions** have been supported since Chrome 125 (API reference, `DebuggerSession.sessionId`). OOPIF and worker sessions from `Target.setAutoAttach {flatten:true}` reach `onEvent` with `source.sessionId`, and `sendCommand` accepts the same field.
4. **Restricted targets.** `ExtensionMayAttachToURL` and `ExtensionMayAttachToRenderFrameHost` (`debugger_api.cc`) refuse the following: chrome:// and other WebUI pages (the new tab page included), chrome-untrusted:// and devtools:// pages, any other extension's chrome-extension:// URL, file:// unless the user allows file access, Web Store pages (`IsRestrictedUrl`), hosts blocked by the ExtensionsSettings policy, privileged WebContents, and the PDF content frame. The check runs on **every frame in the tab**: one cross-extension iframe anywhere in the page makes the whole tab unattachable, with the PDF viewer and MimeHandlerView guests exempt. `rfdah.cc` (`OnNavigationRequestWillBeSent`, `RevalidateSessionAccess`) force-detaches a session as soon as the tab navigates to such a URL or gains such a frame. Child targets the extension may not see are hidden: their `attachedToTarget` is dropped, and the target is resumed and detached (`debugger_api.cc`, around lines 983 to 1075).
5. **One client per extension per target** (`kAlreadyAttachedError`). A second session on the same tab is impossible.
6. **Other limits.** The client is untrusted (`IsTrusted` is false, and V8 runs in `kUntrusted` mode), `MayWriteLocalFiles` is false, and `MayReadLocalFiles` follows the file-access toggle. The infobar "<extension> started debugging this browser" appears unless the extension is installed by policy or Chrome runs with `--silent-debugger-extension-api`. If the user dismisses it, the extension detaches. This infobar takes the place of the remote-debugging approval dialog in #630. It does not remove the warning.
7. The documented domain list is Accessibility, Audits, CacheStorage, Console, CSS, Database, Debugger, DOM, DOMDebugger, DOMSnapshot, Emulation, Fetch, IO, Input, Inspector, Log, Network, Overlay, Page, Performance, Runtime, Storage, Target, Tracing, WebAudio and WebAuthn. `Browser` and `SystemInfo` are not on it. The source has no domain allowlist: it gates each handler instead, as described above.

Legend: **Yes** means allowed as fastbrowse sends it. **Moved** means it works once sent to the tab session instead of the browser. **No** means refused. Paths are under `crates/fastbrowse-browser/src`.

## Navigation and lifecycle

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Page.enable`, `Runtime.enable`, `DOM.enable`, `Network.enable` | `session.rs` `ENABLE_DOMAINS`, `prepare_session` | Yes | none needed |
| `Page.navigate` | `page/mod.rs:1240`, `recording.rs:236` (result card, data: URL) | Yes for web, about:blank and data: URLs. Restricted URLs fail in `PageHandler::Navigate` (`page_handler.cc`), and when a page itself goes to one, the session detaches | `chrome.tabs.update(tabId,{url})` can open some chrome:// pages, but the run cannot see them |
| `Page.getNavigationHistory`, `Page.navigateToHistoryEntry` | `page/act.rs:1043,1053`, `page/mod.rs:679` | Yes | `chrome.tabs.goBack` (no entry ids) |
| `Page.setLifecycleEventsEnabled`, event `Page.lifecycleEvent` | `session.rs:1028,1251` | Yes | `chrome.webNavigation` events, coarser |
| `Page.addScriptToEvaluateOnNewDocument` (main world and `worldName`) | `session.rs:1030,1051` | Yes | `chrome.scripting.registerContentScripts` with `runAt: document_start`, in a MAIN or ISOLATED world |
| `Page.getFrameTree`, `Page.createIsolatedWorld` | `session.rs:966,969`, `page/mod.rs:1158,1162` | Yes | `chrome.scripting.executeScript` with `world: ISOLATED` |
| events `Page.frameAttached`, `frameRequestedNavigation`, `frameStartedLoading`, `frameStoppedLoading`, `navigatedWithinDocument` (**recipes**) | recipes `session.rs:1306-1318` | Yes | `chrome.webNavigation.onCommitted` / `onHistoryStateUpdated` |
| `Page.javascriptDialogOpening` / `Closed`, `Page.handleJavaScriptDialog` | `session.rs:1129,1339,1350` | Yes | none: no extension API answers an alert or confirm dialog |

## Script, DOM and the element tree

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Runtime.evaluate` (snapshot, hit tests, geometry, liveness probe) | `page/mod.rs:1435-1442`, `page/act.rs:370,557`, `session.rs:919-980`, `cdp.rs:412` | Yes (V8 untrusted mode, normal evaluation works) | `chrome.scripting.executeScript` (all frames, MAIN or ISOLATED world). This needs host permissions and cannot reach a context by id |
| `Runtime.addBinding`, event `Runtime.bindingCalled` | `session.rs:1029,1342` | Yes | content script plus `chrome.runtime.sendMessage` |
| `Runtime.releaseObject`, event `Runtime.executionContextCreated` | `page/act.rs:561`, `session.rs:1239` | Yes | n/a |
| `DOM.describeNode` (frame behind a hit) | `page/act.rs:560` | Yes | read the iframe's `src` from JS, or `webNavigation.getAllFrames` |
| `Runtime.runIfWaitingForDebugger` | `session.rs:1056,1400` | Yes on child sessions | n/a |

## Input

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Input.dispatchMouseEvent` (click, move, wheel, drag) | `page/act.rs:1036-1292` | Yes. The events are trusted (`isTrusted`), as they are today | none: script-built events are untrusted |
| `Input.dispatchKeyEvent`, `Input.insertText` | `page/act.rs:1027,847` | Yes | none |
| `Input.cancelDragging` | `page/act.rs:1280` | Yes. Drags that carry files need file access (`input_handler.cc`) | none |

## Screenshots, screencast and recording

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Page.captureScreenshot` | `page/mod.rs:1066`, `recording.rs:306` | Yes when taken from the surface, which is the default. `fromSurface:false` needs a trusted client (`page_handler.cc`) | `chrome.tabs.captureVisibleTab`: visible tab only, rate-limited, needs `<all_urls>` |
| `Page.startScreencast`, `stopScreencast`, `screencastFrameAck`, event `screencastFrame` | `recording.rs:462-464`, `session.rs:819,833,1351,1522,1633,1687` | Yes for the page. Casting the browser window is refused (`page_handler.cc`) | `chrome.tabCapture` / `getMediaStreamId` plus an offscreen document (video, not JPEG frames) |
| Paint keeper: a second `Target.attachToTarget` on the same tab to run a one-frame cast (`show`, `wake`) | `session.rs:806,819,833` | **No**: `attachToTarget` is refused, and a second client gives `kAlreadyAttachedError` | Run the keep-painting cast on the one session, or rely on focus emulation alone |
| `Emulation.setFocusEmulationEnabled` | `session.rs:1041` | Yes | none |
| `Emulation.setDeviceMetricsOverride` (**recipes**) | recipes `session.rs:1089` | Yes | `chrome.windows.update` to resize (whole window) |
| `Page.getLayoutMetrics` | `page/act.rs:374` | Yes | JS `visualViewport` |

## Frames, targets and popups

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Target.setDiscoverTargets` | `session.rs:407,411` | **No** | `chrome.tabs.onCreated` (`openerTabId`), `onUpdated`, `onRemoved`, `webNavigation.onCreatedNavigationTarget` (`sourceTabId`) |
| `Target.getTargets` | `session.rs:839` | **No** | `chrome.debugger.getTargets()` (has `tabId`, `url`, `title`, `attached`) or `chrome.tabs.query` |
| `Target.createTarget` (`background`) | `session.rs:994` | **No** | `chrome.tabs.create({url, active:false})`, then `debugger.attach` |
| `Target.attachToTarget` (`flatten`) | `session.rs:806,895,915,997,1569` | **No**. The extension host also checks the target (`SendMessageToBackend`) | `chrome.debugger.attach({tabId})`, once for each tab |
| `Target.setAutoAttach` on the browser (popup pause with `filter: page`) | `session.rs:422` | **No**: there is no browser session, and the tab auto-attacher does not cover new tabs | none that pauses a new tab before its first request |
| `Target.setAutoAttach` on a page (OOPIFs, workers) | `session.rs:1031` | Yes (Chrome 125+). Hidden children are dropped | n/a |
| `Target.detachFromTarget` | `session.rs:924,1401` | Yes for child sessions. Use `chrome.debugger.detach` for tabs | n/a |
| `Target.getTargetInfo` (by id, from the browser) | `session.rs:549,558` | **Moved**: works only for the session's own target, sent on that tab's session | `chrome.tabs.get(tabId).url`, which still answers while the renderer is hung |
| `Target.activateTarget` | `session.rs:791` | **No** | `chrome.tabs.update({active:true})` plus `chrome.windows.update({focused:true})` |
| `Target.closeTarget` | `page/mod.rs:1216`, `session.rs:494` | Yes for the own or an auto-attached target only | `chrome.tabs.remove` |
| events `Target.attachedToTarget`, `detachedFromTarget`, `targetCreated`, `targetInfoChanged`, `targetDestroyed` | `session.rs:1274-1325` | Only child sessions arrive, filtered and synthesized by the extension host. No browser-wide stream exists | `chrome.tabs` / `webNavigation` events |
| event `Page.windowOpen` | `session.rs:1249` | Yes | `webNavigation.onCreatedNavigationTarget` |

## Downloads and PDF

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Fetch.enable` (Document requests at the Response stage, plus the Request stage for scoped runs) | `session.rs:1032`, patterns at `:125,:131` | Yes, for the tab and its auto-attached children | `declarativeNetRequest` can block but not read |
| event `Fetch.requestPaused`, `Fetch.continueRequest`, `Fetch.failRequest` | `session.rs:469,1383,1529,1548,1687` | Yes | DNR block rules for scope refusal |
| `Fetch.getResponseBody` (download capture) | `session.rs:1712` | Yes | `chrome.downloads` reports the file and its path but cannot read the bytes. A `fetch()` from the extension with the user's cookies can re-download them |
| PDF text (**recipes**): the same Fetch capture, extended to `application/pdf` documents and read in `pdf.rs`'s helper process | recipes `session.rs:1766,2011`, `pdf.rs` | Yes. The body is read before the viewer loads, and the viewer frame stays hidden (`IsPdfExtensionPrincipal` skip in `debugger_api.cc`) | n/a |
| Saving to disk | none (Fetch continues and Chrome saves the file) | `Browser.setDownloadBehavior` is refused because `MayWriteLocalFiles` is false (`browser_handler.cc`). fastbrowse does not call it | `chrome.downloads.onCreated` with `cancel`/`erase` to stop the duplicate save in the user's profile |

## Network

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Network.setBypassServiceWorker` | `session.rs:1066` | Yes | none |
| events `Network.requestWillBeSent`, `loadingFinished`, `loadingFailed` | `session.rs:1253-1274` | Yes | `chrome.webRequest` (observe only) |
| events `Network.responseReceived`, `dataReceived` (**recipes**) | recipes `session.rs:1342-1357` | Yes | `chrome.webRequest.onResponseStarted` (no byte counts) |

## Browser and system

| Method | Where | chrome.debugger | Non-CDP cover |
|---|---|---|---|
| `Browser.getVersion` (liveness check, browser level) | `cdp.rs:423` | **Moved**: the tab session's `BrowserHandler` answers it, though the docs do not list the domain. In an extension the transport itself shows liveness | `navigator.userAgentData`, `chrome.runtime` port liveness |
| `SystemInfo.getProcessInfo` (browser pid for the desktop cursor overlay) | `page/act.rs:359` | **No**: "only supported on the browser target" (`system_info_handler.cc`) | none. `chrome.windows.get` gives the window bounds but no pid |

## Gaps that break a fastbrowse run

1. **Transport.** All browser-level traffic (`session_id: None`) has nowhere to go. `cdp.rs` and `session.rs` need a backend that maps `Target.*` to `chrome.tabs` and `chrome.debugger.attach`, plus a relay, either native messaging or a local WebSocket the extension dials. Prior art: Puppeteer's `ExtensionTransport` and the Playwright MCP bridge extension both relay CDP over `chrome.debugger` (check them before designing, per the consult-open-source-harnesses rule).
2. **Scoped popup gate.** Scoped runs rely on browser `setAutoAttach {waitForDebuggerOnStart}` so a popup's first request cannot go out before Fetch blocks it. Nothing can replace that pause: by the time `tabs.onCreated` arrives and `debugger.attach` runs, the first request may already be on the network. Workaround: `declarativeNetRequest` session rules that block `main_frame`/`sub_frame` outside the grant for the run's tabs (`tabIds`) and for requests the granted origins start (`initiatorDomains`). Fetch stays as the second check.
3. **Restricted tabs and frames.** A user's tab cannot be attached while it shows chrome://newtab or another WebUI page, or while any frame in it comes from another extension (password managers and Grammarly-style iframes). The session is also dropped mid-run when such a frame appears or the page moves to one. Workaround: open run-owned tabs with `chrome.tabs.create`, navigate a WebUI tab with `tabs.update` before attaching, and treat `onDetach` as a recoverable "page became restricted" error that names the cause. The Bitwarden adapter's inline menu is one such iframe, so check it.
4. **Background painting.** The paint keeper needs a second session, which an extension cannot have. Without it, a background tab can stall screenshots. Workaround: use one cast per tab (fold the keep-painting cast into the live or recording cast, and restart it after a renderer swap), keep focus emulation, and as a last resort activate the tab with `tabs.update`. That last resort moves the user's window.
5. **Infobar.** The approval dialog becomes the "started debugging this browser" bar on every attached tab. Dismissing it ends the run. Only policy install or `--silent-debugger-extension-api` hides it.
6. **Desktop cursor overlay.** It loses the browser pid from `SystemInfo.getProcessInfo`. Use window bounds from `chrome.windows.get` and find the process another way, or turn the overlay off on this backend.
7. **Downloads land in the user's profile.** CDP capture still works, but Chrome then saves the file too, maybe with a Save As prompt. Cancel or erase it through `chrome.downloads`, or fail the request after capture.
8. **Hung renderer recovery.** `active_url` and `wedged` use browser-level `getTargetInfo`. `chrome.tabs.get` covers the URL. Opening a replacement tab goes through `tabs.create`.

Everything else (Runtime, DOM, Input, Page navigation, dialogs, screenshots, screencast, Emulation, Fetch, Network, OOPIF child sessions on Chrome 125+) works as it is, sent on the tab's session.
