# Electron attach prototype

Reference only. This branch stays unmerged.

The question was whether fastbrowse can drive an Electron app over CDP. It can. The stock `BrowserSession`
fails at `Target.createTarget` (CDP -32000), because Electron has no tab strip to create a tab in.
`AttachedSession` in `electron_attach.py` attaches to a window the app already has and leaves the rest of the code
untouched. Against the fixture app, fills, clicks, preload IPC, a second `BrowserWindow`, `confirm()` and screenshots
all worked through the real `CdpPage`. In VS Code it observed the workbench and opened a file from the explorer.

## Run it

```sh
cd prototypes/electron-fixture && npm install
# T3 Code and other Electron hosts set ELECTRON_RUN_AS_NODE, which starts the app as plain Node.
env -u ELECTRON_RUN_AS_NODE npx electron . --remote-debugging-port=9333 &
cd ../.. && PYTHONPATH=. uv run python prototypes/electron_attach.py 9333
```

For VS Code, launch it with a throwaway `--user-data-dir`, `--extensions-dir`, and `--remote-debugging-port=9334`,
then run `prototypes/vscode_observe.py 9334`.

## What a real change needs

- An attach mode on `BrowserSession` that takes an existing page target and adds nothing to `_owned`, so teardown
  closes no app window.
- Adoption of new windows that have no `openerId`. Electron's main process opens them, so the popup rule never
  matches.
- The init scripts evaluated on attach. `addScriptToEvaluateOnNewDocument` only reaches the next document.
- An Electron launcher adapter that unsets `ELECTRON_RUN_AS_NODE` and kills the app on exit. The app keeps its state
  between attaches, so a clean run needs a fresh process.
- A check of `file://` and `vscode-file://` origins in `safety.py` and the agent's start and back logic.
- Some way to read Monaco text. VS Code marks the rendered editor lines `aria-hidden`, so `viewport_text` misses
  file content the screenshot shows.

Not tested: a full `run()` with a model.
