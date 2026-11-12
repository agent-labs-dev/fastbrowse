---
title: Connect an agent to a running Chrome over CDP
description: "Drive a Chrome you already have open through the extension, or an Electron app over its DevTools port from an SDK: picking a window and keeping a signed-in browser open."
topic: Browsers and logins
published: 2026-11-01
updated: 2026-11-12
verifiedAgainst: v0.10.0
verifiedOn: 2026-11-12
---

fastbrowse can drive a browser you already have open, signed in with your profile. From the command line that is your own Chrome, through the extension. Any other Chromium app, including an Electron app, needs its DevTools port and an SDK.

## Drive a window in your own Chrome

The command line runs in your own Chrome through the [fastbrowse extension](https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop). Run `fastbrowse extension install` once and add the extension from the Chrome Web Store. Then name the window the run should use:

```sh
fastbrowse "Report my open orders." --start https://www.amazon.com/ --target-match amazon
```

- `--target-match` picks a window by title or URL, and implies `--attach`.
- `--attach` drives the window you have open instead of a tab of its own.
- Without `--start`, the run begins on whatever the window shows, and the window stays open afterwards.
- Popups the window opens join the run.

Without `--attach`, each run works in a tab group of its own and closes its tabs afterwards. A run that stops for you to confirm a step, fill in a value or sign in leaves that tab open, so you can finish in the page it filled. The agent never holds the password, because the session is already in your Chrome. For runs that need credentials handed over, see [browser agent logins without sharing passwords](/blog/agent-credentials-scoped/).

## Connect an Electron app or a debugging port from an SDK

The command line refuses `--cdp-url` and `--cdp-port`. An Electron app, or a Chrome you start with a debugging port, has no extension, so connect from the Python, Node or Go SDK with `cdp_port=` or `cdp_url=`.

`--remote-debugging-port` is read at launch. A running app ignores it, so quit the app and start it again with the flag.

```sh
open -a "Visual Studio Code" --args --remote-debugging-port=9222 ~/code/my-project
```

On Linux and Windows, pass the same flag to the binary. Then point a run at that port:

```python
from fastbrowse import run_task

await run_task(
    "Open the Extensions view and report how many extensions are installed.",
    cdp_port=9222,
    attach=True,
    target_match="my-project",
)
```

Chrome can still ask you to allow remote debugging on each connection, and the run waits for that approval. Chrome ignores the debugging port on its default profile, so for a signed-in browser that stays open, give it a data directory of its own, sign in once by hand and leave it running:

```sh
google-chrome --remote-debugging-port=9222 --user-data-dir="$HOME/.fastbrowse/chrome"
```

Without `attach=True`, each run opens a background tab in that browser and closes it afterwards.

## Focus while a run works

A run in a visible window works in a tab behind the current one and does not raise the window. Three things still bring it forward:

- Chrome raises its window when it starts headed.
- A popup opens a new window at the front.
- With `--attach` or `attach=True`, clicking a tab in that window makes the run follow your click.

To keep a run out of sight, run it headed in a separate session.

## Close the port when you are done

A DevTools port is for SDK runs. Anything on the machine that can reach one can read every page, cookie and signed-in session in that browser. Bind the port to loopback only, and close the app or restart it without the flag when you finish.

[The attach guide](/docs/attach/) is the reference for the flags, and [run statuses and exit codes](/docs/results/) lists how a run ends.

Next: [your first run](/docs/getting-started/).
