---
title: Run a browser agent in your own signed-in Chrome
description: "Install the fastbrowse extension, check it is connected, run a read-only task with --extension, and stop it. What the run can reach and what Chrome shows."
topic: Browsers and logins
published: 2026-11-12
updated: 2026-11-12
verifiedAgainst: v0.11.0
verifiedOn: 2026-11-12
---

A fresh browser has none of your sign-ins. The fastbrowse extension lets a run work in the Chrome you already use, where you are signed in. The run opens tabs of its own in that browser; it does not use the tabs you already have open. No Browser Use key is involved: the run uses your own browser and your own model key.

## Set up

You need [uv](https://docs.astral.sh/uv/), Chrome 125 or later (`fastbrowse extension install` also registers Edge, Brave and Chromium) and a model key. `OPENROUTER_API_KEY` covers the model calls and the Jev decision calls.

```sh
uv tool install fastbrowse
export OPENROUTER_API_KEY=...
fastbrowse extension install
```

`fastbrowse extension install` registers fastbrowse with every Chromium browser it finds for your user, then prints the store link. It writes a small launcher and a native-messaging manifest into your user folders, and needs no admin rights. Run it again if you move or reinstall fastbrowse.

Next, add the extension from the [Chrome Web Store](https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop), then restart Chrome or turn the extension off and on at `chrome://extensions` so it connects. The first task below is the check: with no browser connected, the run ends `unavailable`. Open fastbrowse from Chrome's Extensions menu to see active runs and their **Stop** buttons.

This build requires extension **0.1.1 or later** with its supported-command list. Store version 0.1.0 cannot connect to this build. If the store does not offer the update yet, run `fastbrowse extension path`, open `chrome://extensions`, enable **Developer mode**, choose **Load unpacked** and select the printed folder.

## Run a read-only task first

Browser tasks use the extension by default. `--extension` makes that choice explicit.

```sh
fastbrowse "What is the title of the top story?" \
  --start https://news.ycombinator.com/ --extension --browser
```

The run opens a tab group named `fastbrowse: <your task>`, works in it, and prints the answer with the quotes it came from. Only `complete` is a success, and the command exits 0 only for `complete`.

With no browser connected, the run ends `unavailable` before it spends anything. It never falls back to another browser.

## What happens to your tabs

- A run works only in tabs it opened, grouped together. It does not touch the tabs you had open.
- When it ends, it closes its tabs. If it stops at `needs_confirmation`, `needs_input` or `needs_login`, it leaves the tab it stopped in open, outside the group, so you can finish the step.
- While it is attached, Chrome shows a bar saying that an extension started debugging the browser. This bar is Chrome's own and cannot be hidden by the extension.

## Stop a run

Any of these ends the run `cancelled` (exit 12), and the error says which one:

- **Stop** next to the run in the extension popup.
- **Cancel** on Chrome's debugging bar. That bar covers the whole browser, so it stops every run.
- Closing all of the run's tabs.

The result keeps the steps taken and the money spent.

## What the run can reach

The extension relays page-level commands to the tabs the run opened: clicks, typing, navigation, reading the page and screenshots. It cannot read cookies, saved passwords or other sites' storage through browser APIs. fastbrowse accepts connections only from the extension's ID, over a socket only your user can open.

The extension itself talks only to fastbrowse on your computer. fastbrowse sends the pages a run reads to the model providers you configured: page text and screenshots to the LLM and to Jev, which `OPENROUTER_API_KEY` routes through OpenRouter. Pages you are signed in to count. Choose tasks and providers with that in mind.

Two more things to know:

- **Authorization is a check, not a guarantee.** An action classified as irreversible, such as a purchase or a sent message, waits for `--authorize`, and the run stops at `needs_confirmation` without it. The classifier can miss a change, so the first tasks should be read-only, and `--authorize` should wait until you trust the task.
- **Other extensions see the run's pages.** The run's tabs are ordinary tabs. An extension with site access reads them like any other tab, and an extension that adds a frame to every page, such as a screen recorder or a password manager, makes Chrome end the run `unavailable`. Set that extension's **Site access** to **On click** at `chrome://extensions`. For sensitive work, use a separate Chrome profile with only fastbrowse installed.

## More than one browser

With the extension in more than one browser, give each a name in its popup, then pick one:

```sh
fastbrowse "What is the title of the top story?" \
  --start https://news.ycombinator.com/ --extension --extension-browser work --browser
```

## Remove it

`fastbrowse extension uninstall` removes what `install` wrote. Remove the extension itself at `chrome://extensions`.

For other ways to reach a browser, see [attaching to a running Chrome](/blog/attach-to-running-chrome/).

For connection errors, see [the extension connection checks](https://www.fastbrowse.ai/docs/troubleshooting/#the-extension-is-not-connected).
