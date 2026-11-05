# What Chrome shows or requires for chrome.debugger, native messaging and extension install

Research for #787, feeding the extension backend in #630 (map #784). It answers four questions: what `chrome.debugger.attach` shows the user, how a native-messaging host is registered per browser and OS, what each extension install route costs the user, and what changed from Chrome 136 to current stable.

Sources: Chromium `main` at `31b20f15b2fc` (Chrome 159, 2026-11-05), with the load-bearing lines rechecked on the current stable tag `157.0.8103.38`; brave-core `master` at `24edf1f09bb0`; developer.chrome.com and learn.microsoft.com pages linked inline. Links below use `CR` for `https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/`. Nothing here was run against a real browser.

## Answer in brief

- `chrome.debugger.attach` raises **no dialog**. It shows one infobar per extension, "`<name>` started debugging this browser", with a Cancel button. Cancel detaches the extension (`canceled_by_user`).
- The infobar is skipped in exactly two cases: Chrome launched with `--silent-debugger-extension-api`, or the extension **installed by enterprise policy** (`ExtensionInstallForcelist` or `ExtensionSettings` force install). No manifest key hides it.
- The "Allow remote debugging?" dialog belongs only to the chrome://inspect remote-debugging server (Chrome 144+). It fires on each WebSocket connection to `/devtools/browser/...` on that port. `chrome.debugger` never goes through that server, so attaching neither triggers the dialog nor changes its logic.
- So a policy force-installed extension plus a native-messaging host is the only route found to zero dialogs **and** zero infobars on the user's own Chrome. On an unmanaged machine that route works on Linux with any update URL (root writes a JSON file), on Windows only for a Chrome Web Store hosted extension (admin writes HKLM), and on macOS only through a configuration profile the user approves.

## 1. chrome.debugger: infobar, not dialog

**What attach shows.** `ExtensionDevToolsClientHost::Attach()` attaches the client to the agent host, then creates the warning unless it is suppressed ([CR `chrome/browser/extensions/api/debugger/debugger_api.cc` l.715 to 748](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/api/debugger/debugger_api.cc#715)):

```cc
// We allow policy-installed extensions to circumvent the normal
// infobar warning. See crbug.com/41302695.
const bool suppress_warning =
    base::CommandLine::ForCurrentProcess()->HasSwitch(
        ::switches::kSilentDebuggerExtensionAPI) ||
    Manifest::IsPolicyLocation(extension_->location());
```

`Manifest::IsPolicyLocation` is true only for `kExternalPolicy` and `kExternalPolicyDownload` ([CR `extensions/common/manifest.h` l.81](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/extensions/common/manifest.h#81)). Web Store, unpacked, external-pref and registry installs all get the infobar. The same lines are present at the 157 stable tag (`debugger_api.cc` l.602 to 606).

**Infobar behaviour** ([CR `chrome/browser/extensions/api/debugger/extension_dev_tools_infobar_delegate.cc`](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/api/debugger/extension_dev_tools_infobar_delegate.cc)):

- Text `IDS_DEV_TOOLS_INFOBAR_LABEL`: `"<extension name>" started debugging this browser` ([CR `chrome/app/generated_resources.grd` l.4779](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/app/generated_resources.grd#4779)).
- One delegate per extension ID, shown as a `GlobalConfirmInfoBar` across all tabs. A second attach by the same extension reuses it (`Create`, l.45 to 64).
- One button labelled Cancel. Pressing it detaches every session of that extension (l.99 to 116). The API reports `onDetach` with `canceled_by_user` ([debugger API reference](https://developer.chrome.com/docs/extensions/reference/api/debugger), `DetachReason`).
- After the last session detaches, it closes itself after `kAutoCloseDelay = base::Seconds(5)` (`.h` l.30, `MaybeStartAutocloseTimer` l.134). The next attach brings it back. Infobars take page height, so viewport size changes while it is shown.

**Install-time warnings** for the permissions this design needs ([permissions list](https://developer.chrome.com/docs/extensions/reference/permissions-list)): `debugger` shows "Access the page debugger backend" and "Read and change all your data on all websites", `nativeMessaging` shows "Communicate with cooperating native applications". These appear in the Web Store install dialog. Policy installs grant permissions implicitly (`ExtensionInstallForcelist` description, below).

**Avoiding the infobar without the launch flag.**

| Route | Hides infobar | Notes |
|---|---|---|
| Manifest key or permission | No | No such key exists. The only checks are the two above. |
| `ExtensionInstallForcelist` or `ExtensionSettings` with `installation_mode: force_installed` | Yes | Location becomes `kExternalPolicyDownload`. Platform limits in section 3. |
| `--silent-debugger-extension-api` | Yes | Launch flag, so not available on a Chrome the user already started. Also the only way to attach to an extension background page (API reference). |
| `DeveloperToolsAvailability` and its allow and block lists | No | These can only block CDP. Value 2 blocks `chrome.debugger` too ([CR `.../Miscellaneous/DeveloperToolsAvailability.yaml`](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/components/policy/resources/templates/policy_definitions/Miscellaneous/DeveloperToolsAvailability.yaml)). The default 0 only blocks DevTools on the force-installed extension's own pages, not attaching to web tabs from it. |

Policy can also restrict a non-force-installed extension: `ExtensionSettings.runtime_blocked_hosts` makes `attach` fail on those hosts (API reference). A stricter attach-time check (`kExtensionDebuggerStrictPolicyRestrictions`, blocks attach entirely when blocked hosts, screenshot or DLP policy apply) exists but is disabled by default ([CR `extensions/common/extension_features.cc` l.140](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/extensions/common/extension_features.cc#140); `debugger_api.cc` l.1384 to 1433). Policy-installed extensions count as trusted there.

**Where "Allow remote debugging?" comes from.**

1. Feature `kDevToolsAcceptDebuggingConnections` is enabled by default on Windows, macOS and Linux, disabled on ChromeOS ([CR `chrome/browser/devtools/features.cc` l.203 to 210](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/devtools/features.cc#203); same at 157).
2. When neither `--remote-debugging-port` nor `--remote-debugging-pipe` started a server, `RemoteDebuggingServer::GetInstance` watches the local-state pref `kDevToolsRemoteDebuggingEnabled`, which the chrome://inspect#remote-debugging checkbox sets ([CR `chrome/browser/ui/webui/inspect/inspect_ui.cc` l.689 to 710](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/ui/webui/inspect/inspect_ui.cc#689)). When it is on, Chrome starts an HTTP server in `kWithApprovalOnly` mode on the port read from `DevToolsActivePort` in the user data dir ([CR `chrome/browser/devtools/remote_debugging_server.cc` l.185 to 275, 395 to 416](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/devtools/remote_debugging_server.cc#185)). The `RemoteDebuggingAllowed` policy set to false stops it.
3. In that mode `/json*`, the discovery page and frontend resources return 404. A WebSocket to `/devtools/browser/...` calls `delegate_->AcceptDebugging`, any other path gets 403 ([CR `content/browser/devtools/devtools_http_handler.cc` l.604 to 609, 819 to 879](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/content/browser/devtools/devtools_http_handler.cc#819)).
4. `ChromeDevToolsManagerDelegate::AcceptDebugging` calls `DevToolsConnectionDialog::Show` on the last active window, every time ([CR `chrome/browser/devtools/chrome_devtools_manager_delegate.cc` l.565 to 585](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/devtools/chrome_devtools_manager_delegate.cc#565)). The dialog ([CR `chrome/browser/devtools/devtools_connection_dialog.cc`](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/devtools/devtools_connection_dialog.cc)) has title "Allow remote debugging?", body "An external app wants full control over this Chrome session...", and buttons Allow and "Turn off in settings" (`generated_resources.grd` l.4792 to 4806). There is no remembered approval. While a connection is open a global remote-debugging infobar shows (`SetActiveWebSocketConnections`, l.587).

`chrome.debugger` sessions are `ExtensionDevToolsClientHost` objects attached straight to a `DevToolsAgentHost`. They never reach `DevToolsHttpHandler`, so the per-connection dialog and its pref do not apply to them, and attaching does not enable or alter the inspect server.

## 2. Native-messaging host registration

Manifest fields: `name` (lowercase alphanumerics, `_` and `.`), `description`, `path` (absolute on Linux and macOS, may be relative to the manifest on Windows), `type: "stdio"`, `allowed_origins` (exact `chrome-extension://<id>/` entries, no wildcards). The host gets the caller origin as its first argument, and on Windows `--parent-window=<handle>`. Messages are length-prefixed JSON, at most 1 MB from the host and 64 MiB to it. `connectNative` keeps one host process per port, `sendNativeMessage` starts one per message. Callable only from extension pages and the service worker ([native messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)).

### Linux

| Browser | User level | System level |
|---|---|---|
| Google Chrome | `~/.config/google-chrome/NativeMessagingHosts/<name>.json` | `/etc/opt/chrome/native-messaging-hosts/` |
| Chromium | `~/.config/chromium/NativeMessagingHosts/` | `/etc/chromium/native-messaging-hosts/` |
| Edge | `~/.config/microsoft-edge/NativeMessagingHosts/` | `/etc/opt/edge/native-messaging-hosts/` |
| Brave | `~/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts/` | `/etc/opt/chrome/native-messaging-hosts/` (Chrome's) |
| Chrome for Testing (146+) | `~/.config/google-chrome-for-testing/NativeMessagingHosts/` | `/etc/opt/chrome_for_testing/native-messaging-hosts/` |

### macOS

| Browser | User level | System level |
|---|---|---|
| Google Chrome | `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/` | `/Library/Google/Chrome/NativeMessagingHosts/` |
| Chromium | `~/Library/Application Support/Chromium/NativeMessagingHosts/` | `/Library/Application Support/Chromium/NativeMessagingHosts/` |
| Edge | `~/Library/Application Support/Microsoft Edge {Channel}/NativeMessagingHosts/` | `/Library/Microsoft/Edge/NativeMessagingHosts/` |
| Brave | `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/` (Chrome's) | `/Library/Google/Chrome/NativeMessagingHosts/` (Chrome's) |

### Windows

The manifest can live anywhere. The default value of a registry key holds its absolute path. Chrome reads HKCU first (if user-level hosts are allowed), then HKLM, each in the 32-bit view and then the 64-bit view ([CR `chrome/browser/extensions/api/messaging/launch_context_win.cc` l.64 to 92, 185 to 203](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/api/messaging/launch_context_win.cc#64)).

| Browser | Keys read, in order, under HKCU then HKLM |
|---|---|
| Google Chrome | `SOFTWARE\Google\Chrome\NativeMessagingHosts\<name>` |
| Chromium-branded builds | `SOFTWARE\Chromium\NativeMessagingHosts\<name>`, then the Google\Chrome key |
| Edge | `SOFTWARE\Microsoft\Edge\...`, then `SOFTWARE\Chromium\...`, then `SOFTWARE\Google\Chrome\...` (HKLM also under `WOW6432Node`) |
| Brave | Same as Chromium-branded: brave-core does not override `launch_context_win.cc`. That Brave builds with Chromium branding is inferred, not checked. |

Example: `REG ADD "HKCU\Software\Google\Chrome\NativeMessagingHosts\<name>" /ve /t REG_SZ /d "C:\path\host.json" /f`.

Sources: Chrome paths in [CR `chrome/common/chrome_paths.cc` l.459 to 493](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/common/chrome_paths.cc#459) and `chrome_paths_linux.cc` l.80 to 104. Edge in the [Edge native messaging guide](https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging). Brave in [`app/brave_main_delegate.cc` l.141 to 170](https://github.com/brave/brave-core/blob/24edf1f09bb07dcab8ea8e4c3a928a386dd8b7bf/app/brave_main_delegate.cc#L141) (overrides the system dir to Chrome's on both OSes, the user dir to Chrome's on macOS, and to Brave's own user data dir on Linux) and [`chromium_src/chrome/common/chrome_paths_linux.cc`](https://github.com/brave/brave-core/blob/24edf1f09bb07dcab8ea8e4c3a928a386dd8b7bf/chromium_src/chrome/common/chrome_paths_linux.cc).

Rules that bite:

- The user-level dir is `DIR_USER_DATA/NativeMessagingHosts`. A Chrome run with `--user-data-dir` (tests) looks only under that dir, not under `~/.config/google-chrome`. Write the system-level file, or the per-profile file, for test browsers. Channel builds use their own dirs (`google-chrome-beta`, `google-chrome-unstable`, through `GetChannelSuffixForDataDir`).
- Policy `NativeMessagingUserLevelHosts=false` limits Chrome to system-level hosts, and `NativeMessagingBlocklist` and `NativeMessagingAllowlist` can block a host by name.
- Snap and Flatpak Chromium builds sandbox the filesystem and host spawn. Not covered here.

### Pinning the extension ID

The ID is derived from the extension's public key. An unpacked extension without `key` gets an ID derived from its directory path, so it changes per machine. To pin it ([manifest `key`](https://developer.chrome.com/docs/extensions/reference/manifest/key)):

1. Upload the zip to the Chrome Web Store dashboard, without publishing, and copy the public key from the Package tab (View public key).
2. Put it on one line in `manifest.json` as `"key": "MIIBIjAN..."`. Unpacked loads then get the same ID as the store item.
3. List every ID that can reach the host in `allowed_origins`: the Web Store ID, plus the Edge Add-ons ID if published there, since that store assigns its own ([Edge guide](https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging), "Extension IDs required for both stores"). Edge and Brave installing from the Chrome Web Store keep the Web Store ID.

A self-hosted CRX (Linux, or a policy force install with a custom update URL) takes its ID from the `.pem` used to pack it. Pack it with the key whose public half is in `manifest.json` so all routes share one ID. Whether the Web Store accepts an upload that still contains `key` was not checked here; keep it out of the store zip to be safe.

## 3. What each install route costs the user

| Route | Clicks and prompts | Debug infobar | Works on unmanaged stable Chrome |
|---|---|---|---|
| Chrome Web Store | Open listing, "Add to Chrome", then "Add extension" in a dialog listing the warnings above. 2 clicks. | Yes, on each attach | Yes, all OSes. Needs a published, reviewed item. |
| Unpacked, developer mode | chrome://extensions, turn on Developer mode, "Load unpacked", choose folder. About 4 actions. No permission dialog. | Yes | Yes, all OSes. Disabled again whenever Developer mode is off (`DISABLE_UNSUPPORTED_DEVELOPER_EXTENSION`, [CR `extensions/browser/disable_reason.h` l.60 to 62](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/extensions/browser/disable_reason.h#60)). Policy `ExtensionDeveloperModeSettings=1` (128+) or `DeveloperToolsAvailability=2` forbids Developer mode. |
| `--load-extension` | None | Yes | No for Google Chrome: ignored with "--load-extension is not allowed in Google Chrome" ([CR `chrome/browser/extensions/extension_service.cc` l.416 to 445](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/extension_service.cc#416), same at 157). Still honoured by Chromium builds, except with Enhanced Safe Browsing on or the `command_line` install type blocked. Also a launch flag. |
| External extension, Linux: `<id>.json` in `/opt/google/chrome/extensions/` or `/usr/share/google-chrome/extensions/` | Root writes the file. No prompt: installed and enabled at next start. | Yes | Yes. `external_update_url` (Web Store or self-hosted XML) or `external_crx` + `external_version` (local CRX). Google Chrome on Linux has no per-user dir. Chromium also reads `~/.config/chromium/External Extensions/`. |
| External extension, macOS: `<id>.json` in `~/Library/Application Support/Google/Chrome/External Extensions/` or `/Library/Application Support/Google/Chrome/External Extensions/` | No admin needed for the user path. Installed **disabled**, then Chrome shows a "new extension added" alert and the user must enable it. Prompted at most 3 times. | Yes | Web Store `external_update_url` only (no local CRX since Chrome 44). System path must be root-owned with no symlinks. |
| External extension, Windows: `Software\Google\Chrome\Extensions\<id>` with `update_url` under HKLM or HKCU | Same disabled-until-enabled alert as macOS. | Yes | Web Store `update_url` only (no local CRX since Chrome 33). |
| Policy force install (`ExtensionInstallForcelist` `<id>;<update_url>`, or `ExtensionSettings`) | None. Installs silently, grants permissions, user cannot disable or remove it. Chrome then shows as managed by an organisation (observed general behaviour, not traced in source here). | **No** | Linux: yes, any update URL, by root writing JSON under `/etc/opt/chrome/policies/managed/` (Chromium `/etc/chromium/policies/managed/`, Brave `/etc/brave/policies/managed/`). Windows: HKLM `Software\Policies\Google\Chrome`, admin needed; off-store URLs are marked `[BLOCKED]` unless the machine is AD or Entra joined or enrolled in Chrome Enterprise Core. macOS: only forced (configuration profile) preferences count as mandatory; off-store needs MDM, MCX or Chrome Enterprise Core. |

Sources for the table: [alternative installation methods](https://developer.chrome.com/docs/extensions/how-to/distribute/install-extensions) (paths, Win 33 and Mac 44 local-CRX bans, Linux no prompt). External providers per OS in [CR `chrome/browser/extensions/external_provider_impl.cc` l.883 to 925](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/external_provider_impl.cc#883) (Mac system path uses `ENSURE_PATH_CONTROLLED_BY_ADMIN`, per-user dir only on macOS and Chromium-branded Linux) and directories in `chrome_paths.cc` l.58 to 63 and 424 to 455. HKLM and HKCU in `external_registry_loader_win.cc` l.89 to 113. The external-install prompt is on only on Windows and macOS ([CR `extensions/common/feature_switch.cc` l.22 to 27](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/extensions/common/feature_switch.cc#22)), external installs are disabled with `DISABLE_EXTERNAL_EXTENSION` until acknowledged, and `kMaxExtensionAcknowledgePromptCount = 3` ([CR `chrome/browser/extensions/external_install_manager.cc` l.52, 141 to 190, 270 to 290](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/chrome/browser/extensions/external_install_manager.cc#141)). Install verification is enforced only on branded Chrome for Windows and macOS ([CR `extensions/browser/install_verifier.cc` l.68 to 75](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/extensions/browser/install_verifier.cc#68)), which is why off-store installs fail there. Policy rules in [CR `.../Extensions/ExtensionInstallForcelist.yaml`](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/components/policy/resources/templates/policy_definitions/Extensions/ExtensionInstallForcelist.yaml), the `[BLOCKED]` marking in [CR `components/policy/core/common/policy_loader_common.cc` l.50 to 85](https://chromium.googlesource.com/chromium/src/+/31b20f15b2fcade644e95f67791368056f259044/components/policy/core/common/policy_loader_common.cc#50) (called from `policy_loader_win.cc` l.380), macOS forced vs recommended in `policy_loader_mac.mm` l.126 to 129, and Brave's policy dir in `brave_main_delegate.cc` l.166 to 170.

What this means for fastbrowse:

- **Lowest-friction default:** a Web Store item (2 clicks), plus the CLI writing the user-level native-messaging manifest. The user still sees the debug infobar on each run and can cancel it, and the run must handle `onDetach` with `canceled_by_user`.
- **Zero-UI option:** a policy force install, which also clears the infobar. On Linux it is one root-owned JSON file and allows a self-hosted CRX. On Windows it needs admin and a Web Store item. On macOS it needs a user-approved configuration profile. In every case the browser then reports itself as managed, and the user can only remove the extension by deleting the policy. Treat it as opt-in.
- **Development:** unpacked with a pinned `key`. `--load-extension` cannot be used against Google Chrome.

## 4. Changes from Chrome 136 to current stable (157)

| Version | Change | Effect here |
|---|---|---|
| 136 | `--remote-debugging-port` and `--remote-debugging-pipe` are ignored on the default user data dir in branded Chrome ([blog](https://developer.chrome.com/blog/remote-debugging-port); `IsRemoteDebuggingAllowed` in `remote_debugging_server.cc` l.160 to 183). | A launch-flag CDP port on the user's real profile is gone. This is the reason for the extension route. |
| 137 | `--load-extension` ignored in Google Chrome (`extension_service.cc` l.416 to 423). `--disable-extensions-except` is behind `kDisableDisableExtensionsExceptCommandLineSwitch`. The release in which the first one landed is from memory; the source check is at 157 and 159. | No CLI-side sideload into branded Chrome. |
| 128 (earlier, still relevant) | `ExtensionDeveloperModeSettings` policy. | Admins can block the unpacked route. |
| 144 | chrome://inspect#remote-debugging approval mode: a dialog on each connection, plus an infobar while connected ([Chrome DevTools MCP blog](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session)). | The #630 dialog. Not involved in `chrome.debugger`. |
| 146 | Chrome for Testing has its own native-messaging dirs (native messaging doc). | Test runs on CfT need the CfT paths. |
| 159 (main, not stable) | `kExtensionDebuggerStrictPolicyRestrictions` exists, off by default. | Could later make `attach` fail on managed machines with blocked hosts, screenshot or DLP policy. Policy-installed extensions are exempt. |

No change between 136 and 157 was found to the infobar rule, its suppression conditions or the native-messaging lookup outside Chrome for Testing.
