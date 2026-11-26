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

`demo.sh` and `scripts/check.sh` refuse to start while
`demo-sandbox/nile.json` has a live `nile serve` owner; a TUI attached to it
counts. The guard (`scripts/refuse-live-session`) reads the PID in
`.nile/nile/serve.lock`; a dead PID is a stale lock and does not block. Quit
the TUI/server before running the gate.

## Updating coverage

Add new feature checks to the existing `demo.sh` flow, preferably under
`demo-sandbox/checks/`, and clean up their outputs. Keep pixel checks and
`nile show` assertions scoped to the single demo. `scripts/check.sh` runs the
whole script as the end-to-end gate.
