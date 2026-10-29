# Experiments

Not for merging. These keep the eval experiments reproducible:

- `FASTBROWSE_PLAN`, `FASTBROWSE_GROUNDING`, `FASTBROWSE_READER`, `FASTBROWSE_READ_CONFIDENCE`, `FASTBROWSE_VERIFY`,
  `FASTBROWSE_SHORTCUT` and `FASTBROWSE_THRESHOLDS` switch agent features on and off for the ablation runs.
- `FASTBROWSE_CDP_LOG` writes each DevTools command's start and end time to a file, for the cloud round-trip work.
- `live_harness.py` runs Parallax's live tasks (set `PARALLAX_DIR`) through the build in the current directory, on a
  headed Chrome under `DISPLAY`, and keeps each run's full trace.
- `compare_runs.py` tabulates runs across configs and prints one run step by step.
- `ship_check.sh`, `live_campaign.sh` and `fixture_campaign.sh` are the campaigns, run from worktrees named in each.

Each script reads `OPENROUTER_API_KEY` from the environment.
