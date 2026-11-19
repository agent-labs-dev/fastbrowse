# Parallax pin

The eval plugin pins `parallax-evals[browser]==2.0.0.dev49` in
`evals/pyproject.toml` and `evals/uv.lock`. The dependency resolves directly
from PyPI. No local source override is used.

The pinned version supplies:

- world-less `TaskRecord` sources and `Area.BROWSER`;
- `browser-evidence-v2`, including evidence splitting and screenshots;
- task authorization in the judge state, so an irreversible action is required only when authorized;
- judge identity and retry handling for `environment interruption: `;
- typed timing, cost components, and harness details;
- campaign budgets, reserves, `budget.jsonl`, processes, and run artifacts;
- the browser harnesses and the `plain-cdp` final-page observer.
- the official `bu-bench` and `online-mind2web` sources, their upstream grading
  bridges, and their interruption handling.

The plugin supplies only fastbrowse's three harness entry points, its combined
task source, the pool lease adapter, and the publication mapping into the
unchanged fastbrowse schema 7. Official runs use the same mapping, with the
source from `RunManifest.selection` selecting the public group.

The public fastbrowse wheel does not depend on Parallax. Its `evals` optional
extra enables the offline fixture bench, whose judge interface is a lazy
adapter over `parallax.jev`.

## Desktop arms

`fastbrowse-desktop` and `claude-code-deskpilot` run `evals/tasks/files-desktop-report-cross-app` in Parallax's headless
Hyprland box (`session = "hyprland"`, `launch_by = "harness"`), through `.github/workflows/desktop-eval.yml`. The box needs
the `vkms` kernel module, so the workflow runs on GitHub's own runners, not Blacksmith's.

- `fastbrowse-desktop` runs `fastbrowse --desktop APP... --json` with the task's app commands and reads the result as
  `desktop/bench.py` does. It passes no `--record`: Parallax records the box's display for `--videos`.
- `claude-code-deskpilot` is claude-code with deskpilot's MCP server and claude's own tools off, so it can reach the
  apps only as the fastbrowse arm does. Its prompt carries the launch commands of the task's apps; a test keeps them
  equal.
- The checks read the saved `.ods` and `.odt` and the exported PDF; the controls and mutants run in the task's verify
  image, which holds no LibreOffice. The oracle's files are real LibreOffice saves made by `solution/build_artifacts.py`.
- This needs the Parallax release that carries the Hyprland desktop box (`Desktop.session`, `Desktop.launch_by`,
  `DesktopApp.command`); the pin above predates it.
