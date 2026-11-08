# Demo

`demo.sh` is the one canonical demo. It builds a single project,
`demo-sandbox/nile.json`, and one export, `demo-sandbox/demo.mp4`, from
synthetic media. Keep it current by extending that project or the checks that
feed it; do not add another top-level demo project or a second "showcase"
export.

## Layout

- `demo-sandbox/nile.json` — the demo project. It exercises a transition,
  crop/scale/eq/offset effects, a speed change, multiple video tracks, and an
  audio track. The hero sequence stays first for pixel checks; a longer tail
  gives zoom/pan a realistic range.
- `demo-sandbox/demo.mp4` — the demo's export.
- `demo-sandbox/.demo-media` — validity stamp for generated media. Bump
  `MEDIA_VERSION` in `demo.sh` when generation flags change; the next run
  deletes and regenerates the clips.
- `demo-sandbox/checks/` — transient fixtures for EDL import, detect-edits,
  shell hand-off, pool, undo/register, robustness, and TUI diagnostics. The
  script removes this directory on success, so the sandbox ends with one demo.

## Updating coverage

Add new feature checks to the existing `demo.sh` flow, preferably under
`demo-sandbox/checks/`, and clean up their outputs. Keep pixel checks and
`nile show` assertions scoped to the single demo. `scripts/check.sh` runs the
whole script as the end-to-end gate.
