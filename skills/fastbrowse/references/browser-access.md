# Browser access

## Sign-in and secrets

Prefer a browser or profile the user has already signed into. A local `--profile DIR` preserves cookies across
runs and implies `--local`; a `--cloud-profile ID` uses a Browser Use Cloud profile. Use only a profile the user
has selected. Profile reuse preserves sign-in, not the agent's plan or memory.

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

Look for a user-provided DevTools endpoint or an already enabled local Chrome connection. Chrome's
`DevToolsActivePort` file in its user data directory contains the port and browser websocket path. Read
those two lines and append the path to `ws://127.0.0.1:PORT`; do not read cookies or profile credentials. Some Chrome
versions expose the websocket but return 404 for `/json/version`, so use `--cdp-url` in that case.

Use a discovered or user-provided DevTools endpoint with `--cdp-url URL` or `--cdp-port PORT`. 
fastbrowse opens a task tab and leaves the browser running. Add `--attach` to drive an existing window; `--target-match TEXT` selects a window
by title or URL and implies attach. The selected window stays open after the run, but actions can change it.
Treat cookies and other browser state as belonging to the user. Do not start or expose a debugging endpoint
on their behalf unless the task requires it and they have authorized that access.

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
