# Browser access

## Sign-in and secrets

Prefer the user's own signed-in Chrome, the CLI default, which keeps cookies and sign-ins as they are. A
`--cloud-profile ID` with `--cloud` uses a Browser Use Cloud profile. Use only a profile the user has selected.
A kept local profile or a headless Chrome is an SDK option (`LocalChrome(profile=...)`); the CLI refuses
`--profile` and `--headed`. Profile reuse preserves sign-in, not the agent's plan or memory.

For CLI credentials, have the user configure environment variables outside the conversation, then pass their
names with an explicit origin. Do not read secret values, put them in task text, or print them in diagnostics.

```sh
uvx --from fastbrowse fastbrowse \
  'Sign in using username and password, then report the account plan.' \
  --start https://app.example.com/login \
  --secret username=APP_USERNAME@https://app.example.com \
  --secret password=APP_PASSWORD@https://app.example.com \
  --json
```

Use the real site origin and preconfigured variable names. fastbrowse resolves values only when typing them on
their declared origin; models see secret names. An MCP caller cannot add credentials: its operator declares
them with server `--secret NAME=ENV_VAR@ORIGIN` or `--bitwarden ITEM`. Use only secrets the server offers.
The CLI also supports `--bitwarden ITEM` for a user-selected login from rbw or an unlocked Bitwarden CLI.

## The user's own signed-in Chrome

An ordinary CLI task runs in the user's everyday Chrome, where they are already signed in. `--extension` is the
default and `--local` an alias for it. It reaches Chrome through the fastbrowse extension with no approval
prompt, works in a tab group of its own and closes its tabs when it ends. The user sets it up once with
`fastbrowse extension install` and by installing the
[extension from the Chrome Web Store](https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop);
do not install it for them. Pass `--cloud` for a Browser Use Cloud browser instead.

```sh
uvx --from fastbrowse fastbrowse \
  'Report the delivery date of my most recent order without changing it.' \
  --start https://www.amazon.com/ --json
```

With no browser connected the run ends `unavailable` and names the install step; tell the user rather than
switching to another browser. With the extension in more than one browser, the error lists their labels; pass
the one the user chooses with `--extension-browser LABEL`. A run the user stops in the browser ends `cancelled`:
report that it was stopped and do not retry it.

`--attach` drives a window the user has open instead of a tab of its own; `--target-match TEXT` selects a window
by title or URL and implies attach. The window stays open after the run, but actions can change it. Treat
cookies and other browser state as belonging to the user.

```sh
uvx --from fastbrowse fastbrowse \
  'Report the current account plan without changing it.' \
  --attach --target-match app.example.com --json
```

The CLI refuses `--headed`, `--profile`, `--cdp-url` and `--cdp-port`, and `--viewport` without `--cloud`. For
a DevTools endpoint, such as an Electron app, a headless Chrome or a kept profile, use the Python, Node or Go SDK
(`cdp_url=`, `cdp_port=`, `LocalChrome`). Never look for an endpoint in the user's everyday Chrome, such as its
`DevToolsActivePort` file: Chrome then stops every connection on an "Allow remote debugging?" prompt that waits
for the user's click. Any program on the machine can drive a browser listening on a debugging port, so do not
start or expose one on the user's behalf.

The MCP server chooses its browser connection at startup; inspect the connected tool and ask its operator
to change browser configuration when necessary, rather than passing unsupported CLI flags to `browse`.

## Downloads

For the CLI, pass `--downloads DIR` and include the desired download in the task. Use a directory reserved for
the task. Check the returned download metadata and the resulting files before reporting their paths.
For MCP, check the tool result and server configuration for download access. A path on a remote server is not
automatically a local file the user can open.

## Uploads

For the CLI, pass each file the page needs as `--file PATH` and name it in the task by its file name ("upload
shrike_lid.stl"). A path in the task text alone gives the run nothing to attach. The MCP `browse` tool cannot
attach files.
