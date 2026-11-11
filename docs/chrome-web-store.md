# The Chrome Web Store submission

What the Chrome Web Store asks about the fastbrowse extension, written so it can be pasted into the developer
dashboard, and the archive the store is given. `crates/fastbrowse-browser/extension/` is the whole extension:
nine files, nothing generated, nothing fetched at build time.

The item is reserved. Its id is `ghodfgfhbfaghhfdabopagbfopdjgpop`, fixed by the `key` in `manifest.json` and
by `EXTENSION_IDS` in `crates/fastbrowse-browser/src/extension/protocol.rs`, and
`https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop` is the URL
`fastbrowse extension install` prints. A revision that keeps the `key` keeps the id, so the URL above stays.

## Build the archive

```sh
just extension-zip          # writes dist/fastbrowse-extension.zip
```

The archive holds the nine files at its root, which is the layout the store reads. Bump `version` in
`manifest.json` before a later revision: a store upload must carry a version higher than the one it replaces.

## The listing

| Field | What to write |
| --- | --- |
| Name | `fastbrowse` |
| Category | Developer Tools |
| Language | English (United States) |
| Homepage | `https://www.fastbrowse.ai/` |
| Privacy policy | `https://www.fastbrowse.ai/privacy` |
| Support | `https://github.com/agent-labs-dev/fastbrowse/issues` |

**Short description** (110 characters, and the `description` in `manifest.json`, so keep the two the same):

```
Lets fastbrowse runs work in this browser, where you are already signed in, without a remote debugging prompt.
```

**Detailed description:**

```
fastbrowse is a browser agent. You give it a task in plain language and it works through the page in front of it,
one action at a time, and every claim in its answer quotes the page it came from.

This extension is how a run reaches the browser you already use. Install fastbrowse on the same computer, run
"fastbrowse extension install" once, and then a run started with --extension works here: signed in to everything you
are signed in to, in your own windows, with no remote debugging prompt to approve.

WHAT IT DOES

Each run opens its tabs in a tab group named after its task, so you can see what is working and where.
The toolbar popup lists every run in this browser with the tab count, the elapsed time and the page it is on.
Click a run to bring its tab to the front, or press Stop on it. Stop all ends every run at once.
Closing all of a run's tabs stops that run too, and Chrome's own debugging bar has a Cancel that stops them.
When a run ends it closes the tabs it opened and leaves the ones it handed back to you alone.

WHAT IT CANNOT DO

It relays only the page-level commands a run needs, named in allowed-methods.json, and refuses every other command.
It cannot read cookies, saved passwords or other sites' storage: your signed-in sessions stay in your browser and
are never copied out. It talks to the fastbrowse program on the same computer through Chrome's native messaging,
and to Agent Labs not at all. It has no content scripts and asks for no site access.

GETTING STARTED

1. Install fastbrowse: uv tool install fastbrowse
2. Run: fastbrowse extension install
3. Install this extension, then restart Chrome.
4. Run: fastbrowse "Find my last order's delivery date." --start https://www.amazon.com/ --extension

The extension needs the fastbrowse program on the same computer. Without it the popup says Not connected and a run
ends before it spends anything.

PRIVACY

Nothing the extension handles is sent to us, and no analytics are collected. The tab content a run reads goes to the
program on your computer, which sends it to the model provider you configured. The policy, including a section on
this extension, is at https://www.fastbrowse.ai/privacy
```

## Single purpose

```
Lets the fastbrowse agent, running on your own computer, work in the Chrome browser you are already signed in to.
```

## Permission justifications

Six permissions and no host permissions. Each line below goes into the matching field on the Privacy practices
tab; "needed for functionality" is not a justification the store accepts.

| Permission | Justification |
| --- | --- |
| `debugger` | The extension is the browser end of a run that is driven from the user's own computer. The agent's clicks, typing and page reads reach the tab as Chrome DevTools Protocol commands over `chrome.debugger`, which is the documented API for driving a tab without the "Allow remote debugging?" prompt. Only the 38 methods listed in `allowed-methods.json` are relayed, and each of them acts on page presentation or page content in a tab the run opened or was handed. No command that reads cookies, storage or saved passwords is on the list. |
| `nativeMessaging` | The program that plans and judges a run is `fastbrowse`, installed on the same computer, not a server. Chrome starts it through a native messaging host the user registers with `fastbrowse extension install`, and the extension and that program exchange the run's commands and page snapshots over that channel. Without this permission the extension has nothing to run. |
| `tabs` | A run needs to open a tab, read the address and title of the tabs it is working in, focus a tab when the user clicks the run in the popup, and close the tabs it opened when the run ends. Browser windows opened by a run's page, such as a sign-in popup, are reported to the run and worked in as well. |
| `tabGroups` | Each run's tabs are put in one tab group named after the run's task and coloured, so a person can see at a glance which tabs a run owns and can collapse or move them. The group is expanded when the user clicks that run in the popup. |
| `storage` | Two values are kept in the browser: a random identifier for this browser profile, so a run on the computer can tell two connected browsers apart, and the name the person gives the browser in the popup, which is how `--extension-browser` picks one. Both stay on the computer and are removed when the extension is uninstalled. The list of recently finished runs is kept in session storage and goes when the browser closes. |
| `alarms` | Chrome stops an extension's service worker when it is idle, and a run then has nobody to talk to. A half-minute alarm, plus the startup and install events, reconnect the worker to the local program so a run started while the browser sat idle still finds it. |

## Remote code

The extension executes no code it fetches from the network, and loads no script from anywhere but its own package:
there is no remote `script` tag, no `eval`, and no `new Function`. `background.js` and `popup.js` are the whole of
its own logic, both read from the package.

Declare this on the Privacy practices tab: **the extension does relay commands it is given into `chrome.debugger`**,
which the Manifest V3 requirements allow under the Debugger API exemption. The commands come from the `fastbrowse`
program on the same computer, over native messaging, and not from a server: they are the actions the person's own
run is taking in the tab that run opened. The set of commands is limited by `allowed-methods.json`, which ships in
the package and is read from there, so the reach of what can be relayed is visible in the submitted code even though
the parameters of each command are decided at run time by the user's own program.

Answer the Remote Code field as using the Debugger API for this purpose and quote the paragraph above. Answering
"No, I am not using remote code" would misdescribe the extension.

## Data use

The extension collects nothing for Agent Labs. It has no server of its own, sends no analytics, and has no account
of its own. What it handles goes to the `fastbrowse` program on the same computer, which may then send it to the
model provider the person configured.

| Data type | Handled by the extension | How |
| --- | --- | --- |
| Website content | Yes | The text and structure of the page in a tab a run is working in, and the result of the clicks and typing a run performs, are relayed to the local program so the run can decide its next step. |
| Web history | Yes | The address and title of each tab a run opened or was given, and of a tab the run's page opens, are relayed to the local program. A run that asks for the list of open tabs is given the address and title of each open tab. |
| Authentication information | No | No command that reads cookies, storage or saved passwords is relayed, and the extension itself never reads them. A run works inside the signed-in session the tab already has. |
| Personally identifiable information, health, financial, location, personal communications, user activity | No | The extension collects none of these. It types and clicks on the run's instruction; it does not record what the person does. |

Certify on the same tab: the extension does not sell data to third parties, does not use or transfer data for
purposes unrelated to its single purpose, and does not use or transfer data to determine creditworthiness or for
lending. These match the extension section of https://www.fastbrowse.ai/privacy, which says the same in the
reader's words.

## Review instructions

The extension is the browser half of a program that runs on the reviewer's machine, so the steps below are what
make it work. It shows **Not connected** until step 3 has run.

1. Install the agent, which needs Python 3.11 or newer: `uv tool install fastbrowse` (or `pipx install fastbrowse`).
2. Register the browser: `fastbrowse extension install`. It writes a native messaging host manifest for each
   Chromium browser it finds and prints how to add the extension.
3. Add this extension, then restart Chrome so the service worker connects. The popup should say **Connected**,
   with the browser's own name in the Browser name field.
4. Give the agent a model key in the environment, such as `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, or point
   `FASTBROWSE_LLM_PROVIDER` at a local model. A run needs one to plan.
5. Run a task: `fastbrowse "What is the title of this page?" --start https://example.com/ --extension`

A tab opens in a group named **fastbrowse: What is the title of this page?**, Chrome shows its debugging bar, and
the run answers with a quote from the page. The popup lists the run while it works, with a Stop button beside it.
Pressing Stop ends the run.

Uninstall with `fastbrowse extension uninstall`, which removes the host manifest and the launcher.

## Assets

| Asset | Size | State |
| --- | --- | --- |
| Store icon | 128 by 128 PNG | In the package: `icon-128.png` |
| Screenshot, at least one, up to five | 1280 by 800 PNG or JPEG | **Not made yet** |
| Small promo tile, optional | 440 by 280 PNG or JPEG | Not made |
| Marquee promo tile, optional, needed for featuring | 1400 by 560 PNG or JPEG | Not made |

A screenshot should show the extension in use rather than only the popup: the tab group named after a task, and
the popup listing that run with its Stop button. The store rejects a listing with no screenshot.
