# Chrome extension launch kit

Copy for the fastbrowse listing on the Chrome Web Store and the announcements around it. Each item under
[Release checklist](#release-checklist) waits for the maintainer's go-ahead.

Store URL: https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop

Listing limits come from Chrome's developer docs
([images](https://developer.chrome.com/docs/webstore/images),
[listing fields](https://developer.chrome.com/docs/webstore/cws-dashboard-listing),
[best practices](https://developer.chrome.com/docs/webstore/best-listing)). Recheck them on the day of submission.

## What is live and what is not

The listing is live at extension version 0.1.0 with PyPI fastbrowse 0.10.0. The 0.1.0 package has the background
worker, `allowed-methods.json`, the manifest and a minimal popup: the runs in this browser, each with **Stop**, and a
browser name field. It has no icons.

The source tree is at 0.1.1 and adds the popup's **Connected** and **Not connected** status, **Stop all**, recent
runs, setup help links and conflict hints. Copy that depends on them is marked **0.1.1**. Do not publish it as live
until 0.1.1 is approved.

## Short description

The summary is plain text of 132 characters or fewer. It comes from the manifest's `description`, so a different
sentence needs a manifest change and a new extension version.

Published 0.1.0 description (110 characters):

> Lets fastbrowse runs work in this browser, where you are already signed in, without a remote debugging prompt.

The 0.1.1 manifest uses this description, naming the CLI prerequisite (105 characters):

> Run fastbrowse tasks in the Chrome you already use, where you are signed in. Requires the fastbrowse CLI.

## Detailed description

Lead with what it does, then a short feature list. No keyword lists.

```text
fastbrowse is a browser agent that you start from your terminal. This extension lets a run work in the Chrome you
already use, where you are signed in, instead of in a fresh browser that has to log in again.

It needs the fastbrowse command-line tool on the same computer, uv to install it, and an API key for a model
provider such as OpenRouter. No Browser Use Cloud key is needed. Model calls are billed by your provider.

Set up
1. Install uv (https://docs.astral.sh/uv/), then in a terminal:
     uv tool install fastbrowse
     export OPENROUTER_API_KEY=...
     fastbrowse extension install
2. Add this extension to Chrome.
3. Run a task with --extension:
     fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser

How it works
- Each run opens its own tab group, named after the task, and works only in tabs it opened. It does not use the
  tabs you already have open.
- The run's tabs close when it ends. A run that stops to ask you for something leaves that tab open for you.
- Stop a run from the toolbar popup. Closing the run's tabs, or pressing Cancel on Chrome's debugging bar, also
  stops it.

What it can and cannot do
- It passes page-level commands from fastbrowse to the tabs the run opened: clicks, typing, navigation, reading
  the page and screenshots. It does not read your cookies, saved passwords or other sites' storage.
- The extension talks to fastbrowse on your computer through Chrome's native messaging and makes no network
  requests of its own. The pages open in the run's tabs make their usual requests.
- fastbrowse sends page text and screenshots to the model providers you configure, such as OpenRouter. A run reads
  pages you are signed in to, so choose tasks and providers accordingly.
- While a run is attached, Chrome shows a bar saying that an extension started debugging the browser.
- Actions classified as irreversible, such as a purchase or a sent message, wait for --authorize. The classifier
  can miss a change, so start with read-only tasks.
- Other extensions with access to a site can see and change the run's tabs like any other tab. Some add a frame to
  every page and make Chrome end the run; set their Site access to "On click".

Requires Chrome 125 or later. Free and open source:
https://github.com/agent-labs-dev/fastbrowse
```

## Screenshots

Chrome takes 1 to 5 and prefers the maximum. Each is 1280x800 (Chrome currently downscales to 640x400) or 640x400,
square corners, full bleed, no padding. Screenshots should show the extension's real experience, so capture the build
being submitted and keep text large enough to read at 640x400. Use a throwaway profile and public sites, so nothing
personal is visible. Chrome's docs give no caption rule, so check the dashboard for caption fields before drafting any.

| # | Show | Setup |
|---|------|-------|
| 1 | A run in progress | Chrome window beside a terminal that just ran `fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser`, with the tab group `fastbrowse: What is the title...` and Hacker News in it |
| 2 | The popup with a run | Toolbar popup with one run listed with its task, tab count and **Stop**. **0.1.1** adds **Connected** to this shot |
| 3 | The finished run | Terminal showing the answer, next to Chrome with the run's tabs closed |

Promo images are optional. Chrome's guidance is to communicate the brand, avoid text and not use a screenshot. Small
tile 440x280 and marquee 1400x560. A marquee is required for a featured spot. The store icon is the 128x128 PNG in the
extension package (`icon-128.png`); the 0.1.0 package has none, so 0.1.1 must include it.

## GitHub release note

```markdown
## fastbrowse for Chrome

fastbrowse is available in the Chrome Web Store. Runs can now work in the Chrome you already use, where you are
signed in.

You need [uv](https://docs.astral.sh/uv/) and a model API key. No Browser Use key is needed; your provider bills the
model calls.

    uv tool install fastbrowse
    export OPENROUTER_API_KEY=...
    fastbrowse extension install
    # add the extension: https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop
    fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser

Each run works in a tab group of its own and closes its tabs when it ends. Stop it from the extension's popup.

Chrome shows a bar saying an extension started debugging the browser while a run is attached. Other extensions
with site access can see the run's tabs. Actions classified as irreversible wait for `--authorize`, and the
classifier can miss a change, so start with read-only tasks.

Setup guide: https://www.fastbrowse.ai/blog/use-your-own-chrome/
```

## Social drafts

Short post:

> fastbrowse is available in the Chrome Web Store. Run browser tasks from your terminal in the Chrome you already
> use, signed in, in a tab group of its own. Needs uv, the fastbrowse CLI and a model API key.
> https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop

Longer post:

> Browser agents start in a blank browser and need to log in again. fastbrowse can now use the Chrome you
> already have open. Available in the Chrome Web Store.
>
> You need uv and a model API key (OpenRouter shown):
>
> ```sh
> uv tool install fastbrowse
> export OPENROUTER_API_KEY=...
> fastbrowse extension install
> ```
>
> Add the extension, then run a task:
>
> ```sh
> fastbrowse "What is the title of the top story?" --start https://news.ycombinator.com/ --extension --browser
> ```
>
> Each run gets its own tab group and closes it when done. Chrome shows its debugging bar while a run is attached,
> and other extensions can see the run's tabs, so start with read-only tasks.
>
> Store: https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop
> Guide: https://www.fastbrowse.ai/blog/use-your-own-chrome/

## Release checklist

### Drafts

- [x] Listing and announcement copy reviewed against the published 0.1.0 package and the 0.1.1 source.
- [x] `blog/use-your-own-chrome.md` reviewed and registered in `drift.lock`.
- [x] Site and docs setup uses the published 0.1.0 controls, with a first task to verify the connection.

### Before submitting 0.1.1

- [ ] Manifest `description` matches the chosen short description.
- [ ] Package built from the tagged release, with the ID `ghodfgfhbfaghhfdabopagbfopdjgpop` matching `EXTENSION_IDS` in the CLI, and including the icons.
- [ ] Single-purpose statement, permission justifications (`debugger`, `tabs`, `tabGroups`, `nativeMessaging`, `storage`, `alarms`) and privacy practices updated in the dashboard.
- [ ] Privacy policy URL states that page content goes to the configured model providers.
- [ ] Screenshots captured from the submitted build.
- [ ] Clean-machine walkthrough: the setup steps in the guide, run exactly as written, end with a `complete` result.

### Publication (needs the maintainer's say-so for each)

- [ ] Submit for review, then publish once approved.
- [ ] Confirm the store URL resolves and `fastbrowse extension install` prints it.
- [ ] Merge the blog post so the site serves it.
- [ ] Post the GitHub release note.
- [ ] Post the social drafts.
