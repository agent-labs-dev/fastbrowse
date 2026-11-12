---
title: Connect an agent to a running Chrome over CDP
description: "Drive a Chrome or Electron app you already have open over its DevTools port: the launch flag, picking a window, and keeping a signed-in browser open."
topic: Browsers and logins
published: 2026-11-01
updated: 2026-11-12
verifiedAgainst: v0.10.0
verifiedOn: 2026-11-12
---

fastbrowse can drive a browser you already have open, signed in with your profile. Any Chromium app works, including Electron apps, as long as it exposes its DevTools port.

## Start the app with the debugging port

`--remote-debugging-port` is read at launch. A running app ignores it, so quit the app and start it again with the flag.

```sh
open -a "Visual Studio Code" --args --remote-debugging-port=9222 ~/code/my-project
```

On Linux and Windows, pass the same flag to the binary. Then point a run at that port and name the window it should use:

```sh
fastbrowse "Open the Extensions view and report how many extensions are installed." \
  --cdp-port 9222 --target-match my-project
```

- `--target-match` picks a window by title or URL, for apps with more than one window open.
- `--attach` drives the first window already open instead of opening a tab. Chrome can still ask you to allow remote debugging on each connection, and the run waits for that approval.
- Without `--start`, the run begins on whatever the window shows, and the window stays open afterwards.
- Popups the window opens join the run.

## Keep a signed-in Chrome open

Chrome ignores the debugging port on its default profile, so give it a data directory of its own. Sign in once by hand and leave it running:

```sh
google-chrome --remote-debugging-port=9222 --user-data-dir="$HOME/.fastbrowse/chrome"
fastbrowse "Report my open orders." --start https://www.amazon.com/ --cdp-port 9222
```

Each run opens a background tab in that browser and closes it afterwards. A run that stops for you to confirm a step, fill in a value or sign in leaves that tab open, so you can finish in the page it filled. The agent never holds the password, because the session is already in the profile. For runs that need credentials handed over, see [browser agent logins without sharing passwords](/blog/agent-credentials-scoped/).

## Focus while a run works

A run in a visible window works in a tab behind the current one and does not raise the window. Three things still bring it forward:

- Chrome raises its window when it starts headed.
- A popup opens a new window at the front.
- With `--attach`, clicking a tab in that window makes the run follow your click.

To keep a run out of sight, run it headed in a separate session.

## Close the port when you are done

Anything on the machine that can reach a DevTools port can read every page, cookie and signed-in session in that browser. Bind the port to loopback only, and close the app or restart it without the flag when you finish.

[The attach guide](/docs/attach/) is the reference for the flags, and [run statuses and exit codes](/docs/results/) lists how a run ends.

Next: [your first run](/docs/getting-started/).
