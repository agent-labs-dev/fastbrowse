# Browser access

## Sign-in and secrets

Prefer a profile the user has already signed into for fastbrowse. A local `--profile DIR` preserves cookies
across runs and implies `--local`; a `--cloud-profile ID` uses a Browser Use Cloud profile. Use only a profile the
user has selected. Profile reuse preserves sign-in, not the agent's plan or memory.

A kept profile is the setup that needs the user once: they sign in by hand in a Chrome started on that directory,
close it, and every later `--profile DIR` run is signed in, headless, with no window and no prompt. Chrome opens
a profile in one process at a time, so run one task per profile at once.

For CLI credentials, have the user configure environment variables outside the conversation, then pass their
names with an explicit origin. Do not read secret values, put them in task text, or print them in diagnostics.

```sh
uvx --from fastbrowse fastbrowse \
  'Sign in using username and password, then report the account plan.' \
  --start https://app.example.com/login \
  --secret username=APP_USERNAME@https://app.example.com \
  --secret password=APP_PASSWORD@https://app.example.com \
  --json --max-steps 30 --max-dollars 0.25
```

Use the real site origin and preconfigured variable names. fastbrowse resolves values only when typing them on
their declared origin; models see secret names. An MCP caller cannot add credentials: its operator declares
them with server `--secret NAME=ENV_VAR@ORIGIN` or `--bitwarden ITEM`. Use only secrets the server offers.
The CLI also supports `--bitwarden ITEM` for a user-selected login from an unlocked Bitwarden CLI.

## An existing browser or Electron window

Use a DevTools endpoint only when the user names it, with `--cdp-url URL` or `--cdp-port PORT`. Never look for
one in the user's everyday Chrome, such as the `DevToolsActivePort` file in its user data directory: that
endpoint exists only when remote debugging is switched on in `chrome://inspect`, and Chrome then stops every
connection on an "Allow remote debugging?" prompt that takes the user's focus and waits for their click, once
per run. An endpoint that answers 404 on `/json/version` is that kind.

The endpoint that needs no approval is a Chrome the user starts for the purpose, with a data directory of its
own (Chrome ignores the port flag on its default one):

```sh
google-chrome --remote-debugging-port=9222 --user-data-dir="$HOME/.fastbrowse/chrome"
```

They sign in there once and leave it running; each run then passes `--cdp-port 9222`. Any program on the machine
can drive a browser listening on that port, so the choice to start it is the user's. Do not start or expose a
debugging endpoint on their behalf unless the task requires it and they have authorized that access.

fastbrowse opens a task tab and leaves the browser running. The tab opens behind the window's current tab and
the window is neither raised nor focused, so the user can keep working; pass `--foreground` only when they ask
to watch. Chrome itself still brings a window forward when a page opens a popup, and when a new headed window
starts. Add `--attach` to drive an existing window; `--target-match TEXT` selects a window by title or URL and
implies attach. The selected window stays open after the run, but actions can change it, and a click in a
window's front tab activates that window. Treat cookies and other browser state as belonging to the user.

```sh
uvx --from fastbrowse fastbrowse \
  'Report the current account plan without changing it.' \
  --cdp-port 9222 --target-match app.example.com \
  --json --max-steps 30 --max-dollars 0.25
```

The MCP server chooses its browser connection at startup; inspect the connected tool and ask its operator
to change browser configuration when necessary, rather than passing unsupported CLI flags to `browse`.

## Downloads

For the CLI, pass `--downloads DIR` and include the desired download in the task. Use a directory reserved for
the task. Check the returned download metadata and the resulting files before reporting their paths.
For MCP, check the tool result and server configuration for download access. A path on a remote server is not
automatically a local file the user can open.
