# Recipe renderer prototype

This throwaway prototype answers: what should a fastbrowse recipe and its replay run look like when a user needs to review the flow, understand healing, and inspect a recording?

Run it with:

```sh
python3 -m http.server 8765 --directory prototype/recipe-renderer
```

Open `http://localhost:8765/?variant=a`. Use the bottom switcher or the left and right arrow keys to compare:

- `a`: operator list, best for scanning and editing a recipe
- `b`: run timeline, best for explaining replay and healing over time
- `c`: studio split, best for pairing the recording with a selected step

The page contains a clearly labelled, privacy-safe synthetic fallback dataset. Import JSON opens a local multi-file picker for recipe and run-result files, Import video attaches a local HTML video player, and Export JSON downloads the current review bundle. Selecting a step updates the inspector and exact JSON view. Files are not uploaded. The current run JSON has durations but no absolute video timestamps, so the prototype keeps the player surface honest instead of inventing synchronization.

## Library notes

The prototype uses plain HTML and CSS to keep the design question isolated. If this graduates into a product surface, React Flow is a good fit for an optional control-to-page relationship map and is MIT licensed: https://reactflow.dev/index. Radix Primitives are a strong accessible foundation for tabs, dialogs and menus and use the MIT license: https://github.com/radix-ui/primitives/blob/main/LICENSE. Motion for React can add restrained transitions: https://motion.dev/docs/react. Remotion's Timeline is attractive for a richer video editor, but its licensing needs review for a commercial product: https://www.remotion.dev/docs/timeline and https://www.remotion.dev/docs/terms.

Recommendation: start with the split studio layout as the product direction, keep the list view as a compact review mode, and add a timeline only after recordings expose explicit step timestamps. Use Radix for interaction primitives and Motion for small state transitions. Add React Flow only for an optional recipe map, not the primary step review.

## Hosted integration path

The studio styling follows the hosted dashboard tokens in `fastbrowse-hosted` at commit `e3652df18e30e13e32415c9db804833fb9a4648e`: plane `#0d0d0d`, surface `#1a1a19`, raised surface `#232322`, edge `#2e2e2c`, ink `#ffffff`, muted ink `#898781`, and accent `#6366f1`.

When this moves beyond a prototype, keep the renderer as a client view inside the hosted dashboard. Reuse `dashboard/components/ui.tsx` `Card`, `Button`, `StatusBadge`, and `ResponsiveTable`, and mount it from the existing authenticated run detail route at `dashboard/app/app/[slug]/runs/[runId]/page.tsx`. That route already follows the run event stream, refreshes the proved run, and exposes authenticated artifact links. The renderer should consume those run and artifact props, not invent a second backend or hand-roll authentication, event streaming, or artifact URLs.
