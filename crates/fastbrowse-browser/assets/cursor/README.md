# fastbrowse focus cursor

The ember focus brackets and center dot mark the exact action point. The white outline keeps the marker visible
on light and dark pages. Cua Driver draws this artwork directly as a synthetic cursor, leaving the user's pointer
alone. Each animation keeps all its layers visible through its final frame.

Build from the repository root with Python and Cua Driver 0.45.4 or later:

```sh
python3 scripts/cursor_theme.py
cua-driver cursor-theme build crates/fastbrowse-browser/assets/cursor/fastbrowse.focus.lottie \
  --output crates/fastbrowse-browser/assets/cursor/fastbrowse.focus.cua-theme
```

The CLI embeds the compiled artifact and installs the versioned theme id through Cua Driver's local installer on
first use. A theme revision gets a new id so it does not replace artwork used by another live run.

Cua Driver owns motion and rotation. To keep the marker upright and still at rest, with its daemon running:

```sh
cua-driver config set cursor.rotate off
cua-driver config set cursor.float off
```

These settings apply to every Cua Driver cursor. The run sets its own idle timeout to zero, so it stays visible
between actions. Hyprland uses deskpilot's native marker instead.
