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

The name is the `name` in `manifest.json`, and the store will not take a listing title that differs from it without a
new upload, so the title stays `fastbrowse`. The sentence under it carries what the extension does.

**Short description** (115 characters, inside the store's 132, and the `description` in `manifest.json`, so keep the
two the same):

```
Run fastbrowse tasks in your own Chrome, already signed in, in their own tab group. The popup lists and stops runs.
```

**Detailed description:**

```
fastbrowse is a browser agent. You give it a task in plain language and it works through the page in front of it, one action at a time.

This extension is how a fastbrowse run reaches the browser you already use. The agent itself is the fastbrowse program on your own computer; the extension is the half that opens the tabs, clicks, types and reads the page inside the Chrome profile where you are already signed in. No second browser, no signing in again, no cookies copied out.

WHAT IT DOES

Every run opens its tabs in one tab group named after its task, "fastbrowse: ...", coloured so you can tell runs apart. Clicking a run in the popup expands its group and brings its tab to the front.

The toolbar popup lists every run working in this browser: the tab count, how long it has been running, and the page it is on. Each run has a Stop button. Stop all, which appears once two runs are working, ends them together, and the toolbar icon carries the number of runs.

When a run ends it closes the tabs it opened and leaves the ones it handed back to you open. Closing the last tab of a run yourself stops that run. The popup also lists the last five runs that finished and how each one ended.

Chrome shows its own bar saying an extension is debugging this browser for as long as a run is attached. There is no Allow prompt to click, but the bar is there, and Cancel on that bar stops every run in that browser at once.

WHAT IT DOES NOT DO

It relays only the page-level commands a run needs, the 38 named in allowed-methods.json, and refuses every other command. It cannot read cookies, saved passwords or other sites' storage: your signed-in sessions stay in your browser and are never copied out. It has no content scripts and asks for no access to the sites you visit.

It talks only to the fastbrowse program on the same computer, over Chrome's native messaging, and to no server. Nothing it handles is sent to Agent Labs, and it collects no analytics. The tab content a run reads goes to that program, which sends it to the model provider you configured.

WHAT YOU NEED

The extension needs the fastbrowse program on the same computer. Without it the popup says Not connected and a run ends before it does anything.

1. Install the agent, which needs Python 3.13 or newer: uv tool install fastbrowse
2. Register the browser once: fastbrowse extension install
3. Add this extension, then restart Chrome. The popup should say Connected, with the browser's own name in the Browser name field.
4. Give the agent a model key: export OPENROUTER_API_KEY=...
5. Run a task: fastbrowse "Find my last order's delivery date." --start https://www.amazon.com/ --extension

Chrome 125 or newer, including Edge, Brave and Chromium. The Browser name field in the popup names this browser, which --extension-browser matches when more than one is connected.

PRIVACY

The extension stores a random identifier for this browser profile, so a run can tell two connected browsers apart, and the name you give it in the popup. Both stay on your computer and go when you uninstall. The list of recently finished runs is kept in session storage and goes when Chrome closes. No analytics, no advertising, and nothing it handles is sold or shared.

The policy, including a section on this extension, is at https://www.fastbrowse.ai/privacy
```

Every claim above is checked against `background.js` and `popup.js` in this package: the 38 relayed methods are
`allowed-methods.json`, the "last five runs" and the "Stop all from two runs" are `RECENT` and the `stop-all`
visibility rule in `popup.js`, and the debugging bar is the `canceled_by_user` branch in `background.js`. Two things
the copy deliberately does not say: that the extension hides Chrome's debugging bar (it cannot), and that it works
without the local program (it does not).

### What the last revision said, and why it changed

Version 0.1.0 went live with "Lets fastbrowse runs work in this browser, where you are already signed in, without a
remote debugging prompt." The second half is true only in a narrow sense: there is no *Allow* prompt to answer, but
Chrome still shows its bar saying an extension is debugging this browser, and Cancel on that bar stops every run in
that browser. A listing that promises the prompt is gone makes that bar look like a fault. The replacement leads with
where the run happens and puts the bar, and the Cancel rule, in the detailed description where a person can read it
before installing.

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
| Authentication information | Yes | A password or code the person hands their run (`--secret`) passes through the extension once, as the command that enters it into the page's field. No command that reads cookies, storage or saved passwords is relayed, and the extension itself never reads them. A run otherwise works inside the signed-in session the tab already has. |
| Personally identifiable information, health, financial, location, personal communications | Yes | Not sought, but carried by the website content row: a run on a mail, bank, medical, map or account page relays that page's text, so whatever the page shows of these reaches the local program and may go on to the person's model provider. The extension keeps none of it. |
| User activity | No | The extension types and clicks on the run's instruction; it does not record what the person does in the browser. |

Certify on the same tab: the extension does not sell data to third parties, does not use or transfer data for
purposes unrelated to its single purpose, and does not use or transfer data to determine creditworthiness or for
lending. These match the extension section of https://www.fastbrowse.ai/privacy, which says the same in the
reader's words.

## Review instructions

The extension is the browser half of a program that runs on the reviewer's machine, so the steps below are what
make it work. It shows **Not connected** until step 3 has run.

1. Install the agent, which needs Python 3.13 or newer: `uv tool install fastbrowse`, which fetches Python 3.13 itself
   if the machine lacks it (or `pipx install fastbrowse` on a Python 3.13 interpreter).
2. Register the browser: `fastbrowse extension install`. It writes a native messaging host manifest for each
   Chromium browser it finds and prints how to add the extension.
3. Add this extension, then restart Chrome so the service worker connects. The popup should say **Connected**,
   with the browser's own name in the Browser name field.
4. Give the agent one [OpenRouter](https://openrouter.ai/) key: `export OPENROUTER_API_KEY=...`. That single key
   serves both models a run needs, Jev, which picks each action, and the LLM that writes the answer. A provider
   key such as `OPENAI_API_KEY` alone is not enough: it configures neither the provider nor Jev's decision source,
   and the run stops during configuration.
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
