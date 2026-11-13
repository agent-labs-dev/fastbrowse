# Parallax pin

The eval plugin pins `parallax-evals[browser]==2.0.0.dev22` in
`evals/pyproject.toml` and `evals/uv.lock`. The dependency resolves directly
from PyPI. No local source override is used.

Dev21 supplies the generic pieces that fastbrowse previously duplicated:

- world-less `TaskRecord` sources and `Area.BROWSER`;
- `browser-evidence-v1`, including evidence splitting and screenshots;
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
