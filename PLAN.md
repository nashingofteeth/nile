# nile — plan of record

Project and command: **nile**. Status: **M5 shipped; post-M5 polish shipped;
M6 shipped** (presets, bench and the opt-in chunk scheduler/cache; auto stays
single-worker until a workload shows a win). **Next: pre-merge engine
readiness** (sequences, protocol, time maps, annotations, keyframed effects,
render reports, analysis jobs) so `clip-generator` can merge as a domain
toolkit rather than a second engine. This file is the design/roadmap; the
README is the user-facing how-to. Keep it in sync as milestones land.

**Post-M5 additions:** transparent crop/scale masking and the offset effect
(lower tracks show through); a TUI effects panel with one effect per kind,
in-place editing, and a sticky aspect lock; percent speed input (the model
keeps the rational rate); transition retiming (`--frames/--dur/--percent`,
the `x`/`X` frames form, `r` for a rate); LUTs remain CLI-only in the panel;
the media pool preserves its selection, preview clock, and insert cursor
across toggles; the CLI/TUI were split into modules and `Document` pairs the
project with its history; project state (history, clipboard, proxy/black
cache) lives in `.nile/<stem>/`, with an optional directory default in
`.nile/config.json` (`nile default`).

## Vision

A terminal-native NLE: **TUI dashboard + mpv monitor window, vim
keybinds, FFmpeg underneath.** A general-purpose NLE built to be **high
performance with low system requirements**: the Rust process stays small and
responsive (8 GB RAM and 8 threads are plenty) while decode/encode is
delegated to ffmpeg/mpv, using hardware paths when available. Rust, learned
along the way with JS/PHP→Rust explainers per milestone.

**Rejected alternatives:** ffplay as monitor (no IPC), linking libav
(spawn the binary instead), building on MLT, async/tokio (threads
suffice).

## Architecture

```
nile (process)                       mpv window (child)
├── engine lib (EDL, undo, cache)   ├── hw decode (VAAPI / VideoToolbox)
├── TUI (ratatui) ◄─ JSON IPC ─►    ├── edl:// instant preview
└── spawns ffmpeg (export/preview)  └── observed time-pos → playhead
```

| Piece | Decision |
|---|---|
| Crates | `nle-core` (pure model) · `nle-ffmpeg` (wrappers + graph compiler) · `nile` package (binary `nile` + an `nle` symlink alias + TUI + mpv IPC) |
| Project file | `nile.json` (v10, or `.nile/config.json` default) — media pool (embedded ffprobe reports) + **segment tracks**. Exact rational times, atomic saves, validated on load; vim is a legitimate UI for it |
| Preview | mpv `edl://` — instant cuts, EDL time = timeline time. Gaps → cached 10 s black file. Effects, speed changes and transitions → cached proxy renders |
| Export | filter-graph compiler: `trim`/`atrim` + normalization + `concat`; gaps → `color`/`anullsrc`; frame-grid snapping; hw tail; `-/filter_complex` script files |
| Encoders | `cpu` (libx264), `vaapi` (Linux/UHD 630), `qsv` (Quick Sync), `videotoolbox` (macOS) |
| Time | `Rational64` everywhere; floats only at ffmpeg/IPC boundaries |
| Undo | frame-topology history (segment before/after), sidecar v15 |

**Lessons banked in comments:** `-filter_complex_script` removed in ffmpeg
9 → `-/filter_complex`; filter options use `:` not `,`;
`-avoid_negative_ts make_zero`; mpv EDL's 3rd field is **length** not end;
mpv blocks `lavfi://` inside EDL (hence black-file cache).

### Model decisions (settled)
- **Segment model:** a track is a sequence of `Segment = Clip{..} | Gap(dur)`;
  clip start is a prefix sum; overlaps unrepresentable; trailing gap ordinary.
- **No backward compatibility** (detail in AGENTS.md): format bumps reject
  old files; never migrate — rebuild via `./demo.sh`.
- **Media pool** shared by id: `add_asset` / `asset_usage` / guarded `remove_asset`.
- **Playhead anchoring:** normal edits follow the *media frame*; extend
  commits land on the *edge*; undo restores the *pre-edit* position; delete
  lands at the seam (ripple) or the same offset in the resulting gap (plain).
- **Frames are 0-based** everywhere (`f…`, timecode `:FF`).

## Keymap

Canonical keymap lives in **README.md** (the user manual); keep it there
and update it in place. Summary only, for orientation: transport/motion
(`space`, `h/l/H/L`, `f/F`, `j/k`, `w/b`, `n/N`, `gg/G/0/$`, `< >`, counts),
editing (`i/o`, `I/O`, `s`, `_`, `dd/D`, `yy/p/P`, `u`), and extend
(`e/E`, `[ ]`, `< >`).

## Milestones

| # | Scope | Status |
|---|---|---|
| **M0** | Workspace, `probe`/`trim` (cpu/vaapi/qsv/copy), frame-accuracy (PSNR) | ✅ `2d854b3` |
| **M1** | EDL model, project file, media pool, timeline ops, graph compiler v0, export by content | ✅ `e30d1b4` |
| **—** | `demo.sh` playground + clippy cleanup | ✅ `100acdc`, `dac81c4` |
| **M2** | TUI + mpv monitor + vim nav + instant preview + `r` + `:cmd` + `nile edl` | ✅ `344291a` |
| **M3** | Editing for real | ✅ done |
| **M4** | Multi-track + separate audio tracks | ✅ done |
| **M5** | Transitions (`xfade`/`acrossfade`) + per-clip effects + proxy cache | ✅ done |
| **M6** | Export scheduler (3 lanes), segment-parallel, `nile bench`, presets, resume+cache | shipped (scheduler opt-in) |
| **Pre-merge** | Engine readiness for clipgen: sequences, protocol, time maps, annotations, keyframed effects, render reports, analysis jobs | next |
| **M7** | GUI (Tauri+WebCodecs or libmpv) | someday |

### M3 — shipped (beyond the original scope)

- **Model:** segment model; project v2; media pool; ops — trim in/out
  (+ripple), split, delete (gap/ripple, by id/index),
  extend_edge/extend_gap_tail/head, absorb_outward, paste/paste_ripple
  (clip **or** gap), insert_gap, merge-on-delete; frame-topology undo
  sidecar **v4** storing the pre-edit playhead.
- **TUI:** full keymap; **extend mode** (clip & gap head/tail, material-wide
  bounds, `e` replace / `E` ripple, opposite-end delete, leading-gap
  support, strip **overlay**); **media pool** taking over the info pane
  (asset list + probe metadata + monitor preview with full seek/frame/end
  nav, insert/remove/undo); bottom **keys** cheat-sheet + **message**
  panels; playhead anchoring; 0-based frames.
- **Verified:** ~140 tests, clippy clean, `./demo.sh` pixel checks, release
  installed.

## M4 — multi-track + audio (shipped)

- **Model:** tracks are an ordered list of `Track { kind, muted, segments }`;
  helpers `track(ti)` / `video_tracks` / `audio_tracks` / `add_track`;
  validate requires ≥1 video track. Project **v3**.
- **Ops:** every track-resolving edit op is `*_on(project, track, …)`; the
  unsuffixed name wraps the first video track. A stale index returns
  `TrackNotFound` (never panics).
- **Sync ripple:** a length-changing edit on the first video track shifts or
  removes the same span on every other track; overlay/audio edits stay local.
  A transition boundary on another track skips sync with a notice. Multi-track
  edits record as one `Change::Tracks` entry.
- **Export:** one video track with no audio track keeps the classic concat
  path **byte-for-byte**; otherwise each track is padded to the timeline
  length and video layers composite bottom→top with `overlay` (upper gaps
  transparent `black@0.0`/`yuva420p`), audio sums with
  `amix=inputs=N:normalize=0`. Muted tracks are dropped.
- **TUI:** one row per track with a `v#`/`a#` gutter, active row
  highlighted; `t`/`T` cycle and `<N>t` jumps (digits stay vim counts);
  navigation, edits, and the monitor (active-track EDL) all follow the
  active track; `:track add video|audio · mute [N] · rm N`.
- **CLI:** `nile track ls|add|mute|rm`, `nile add --track N`,
  `nile edl --track N`.
- **Verified:** 183 tests, clippy clean, `./demo.sh` multi-track pixel +
  audio checks.

**Deferred:** linked a/v detach; a muted sole video track is a no-op in the
fast path. (The composite preview — layer `overlay` + audio `amix` in the
monitor — landed in M5 as a proxy span.)

## EDL (CMX3600) import — shipped

- **Timecode** (`time.rs`): `Timecode` + `parse_timecode` (colon or
  drop-frame `;`), `timecode_to_frames` (drop-frame skips 2/4 frame numbers
  per minute except every tenth), `frames_to_seconds`.
- **Parser** (`nle-core/src/edl.rs`): `TITLE:`/`FCM:`, event lines (reel,
  channel, transition, source/record timecodes), and `* FROM CLIP NAME:` /
  `* SOURCE FILE:` comments. Cuts only — dissolves/wipes error by event.
- **Build** (`edl.rs::build`): video events → video track 0 (record gaps →
  `Gap`; origin = the earliest event); one audio track per distinct channel;
  audio events whose reel + source match a video event are skipped as
  **linked** (the clip already carries that audio); source TC → file time
  via `--src-offset`.
- **CLI:** `nile import-edl FILE --media DIR [--fps] [--src-offset]
  [--map REEL=PATH]` — probes the directory, resolves reels/clip names
  (exact file/stem, then a unique substring), infers fps/size from the first
  event's asset, and refuses to clobber an existing project without
  `--force`.
- **Verified:** unit fixtures (cuts, drop-frame, gaps, linked audio) +
  `./demo.sh` (import → export → red/gap/blue pixels).

**Deferred:** audio-only EDLs; additive import into an
existing project. (TUI `:edl-import` landed with the action-parity work
below.)

## OTIO import + clip speed — shipped

- **OTIO import** (`nle-core/src/otio.rs`): reads a `Timeline`/`Stack`/
  `Track`/`Clip`/`Gap` document from `serde_json::Value`, dispatching on the
  schema *name* (the part before the `.`) — tags are versioned (`Clip.1`,
  `Clip.2`), so a tagged enum would break on a version bump. `nile import-otio
  FILE [--media DIR] [--fps] [--remap OLD=NEW]` resolves each clip's
  `ExternalReference.target_url`, probes it into the pool, and builds a
  multi-track timeline. Transitions/nesting are **warned**, never silently
  dropped.
- **Rate conforms:** Kdenlive emits **no `LinearTimeWarp`** — it writes the
  clip's `source_range` in *project* frames and the media's `available_range`
  in the *media* rate, so `speed = source_rate / media_rate` and source
  seconds = `frame / media_rate`. Validated on a real 26-min project (27
  clips: 26× 0.8008 + 1× 0.4004, total exactly 37538/24 s).
- **Clip speed** (`timeline.rs`): `Clip.speed` (rational, default 1); timeline
  duration = `(asset_out - asset_in) / speed`. `PROJECT_VERSION` → 4,
  `HISTORY_VERSION` → 7 (the `Atom` carries **exact** source times + speed, so
  undo/redo is bit-for-bit; an earlier frame-count form quantized source
  seconds and slowly drifted speed clips).
- **Editing:** timeline↔source deltas scale by speed (`source_span`, or
  `Clip::source_at`/`timeline_offset_of`); a speed change **always ripples**
  (`set_speed` / `:speed` / `nile speed`, and the `R` key) and lands the new
  duration on the frame grid, storing the rate that fits (`source / duration`)
  with the source span untouched.
- **Repair:** `nile purge-stumps [--track N]` / `:purge` drops sub-frame stumps
  and snaps a drifted timeline back onto the frame grid (cumulative snap, so
  cuts stay within half a frame).
- **Export:** `setpts=(PTS-STARTPTS)/speed`, `atempo` (chained outside its
  range, e.g. 0.4× → 0.5 × 0.8008), source-span trim; the 1× graph stays
  byte-identical. Preview plays a speed clip from a rendered proxy (mpv's
  EDL has no rate).

**Deferred:** OTIO transitions/nesting; audio-only OTIO.

## Edit detect (`detect-edits`) — shipped

- **Detect** (`nle-ffmpeg/src/detect.rs`): one ffmpeg pass —
  `blackdetect` + `select='gt(scene,T)',showinfo` — parsed from stderr.
  `detect` (process) is split from `build_args`/`parse_log` (pure) so the
  parsing is unit-tested without spawning ffmpeg.
- **Reconstruct** (`nle-core/src/detect.rs`): raw cuts/blacks → exact,
  frame-snapped `Span`s (media/gap); adjacent gaps coalesce; sub-`min_shot`
  media folds into a neighbour. Pure, no ids.
- **CLI:** `nile detect-edits FILE [--list --threshold --min-shot
  --black-min --no-gaps --fps --video-only --force]` — a video track plus a
  matching split audio track, written with `save_and_reset_history`.
  `--append [--track N]` adds the shots to an existing project at the
  timeline end instead (audio mirrored onto the first audio track).
- **Verified:** pure unit tests + `./demo.sh` (red/blue/black/green file →
  detect → export → pixel checks; the black span returns as one gap). Also
  run on a 1.4 GB 1080p60 dailies `.mov`: 19 cuts + 2 gaps, including 3 s
  of leading black and a 0.2 s black flash that scene detection missed.

**Deferred:** matching shots back to pooled source
assets (frame fingerprinting) — a rendered file can't recover its sources,
only the cut structure. (TUI `:detect` landed with the action-parity work
below.)

## CLI/TUI action parity — shipped

- **The action catalog** (`nle-core/src/action/mod.rs`): one `Action` enum with
  explicit addressing (`--at` position, `--clip` id, `--seg` index — no
  hidden playhead) and one `run_action` executor over `Project` + `History`
  + the yank register. Three front-ends build the same values: TUI keybinds
  (high-frequency actions only), the TUI `:cmd` line (**every** action), and
  CLI subcommands (every action).
- **Range operations are in the catalog:** `DeleteRange`/`YankRange`/
  `PasteRange` back visual `d`/`y`/`p` and the range flags on `nile delete`,
  `yank`, and `paste`. The shared register may hold a range; normal `p`
  refuses it and `:paste --range` consumes it.
- **One grammar:** `Action::parse` is the `:cmd` syntax and mirrors the CLI
  flags (`:delete --at 3 --ripple` ≡ `nile delete --at 3 --ripple`), with
  `--at`/`--track`/`--clip` defaulting to the playhead / active track /
  clip-under in the TUI. `to_action` is the single CLI `Cmd`→`Action`
  mapping. Coverage tests walk `Action::slot`/`SLOTS` and fail if either
  spelling misses a variant — parity is enforced, not promised.
- **Shared session state:** the undo sidecar persists **both** stacks
  (`HISTORY_VERSION` 9), so `nile undo`/`nile redo` work across processes and
  on TUI-made edits; the yank register (`.nile/<stem>/clipboard.json`) is shared —
  `nile yank` ↔ `yy`, `nile paste` ↔ `p`.
- **New CLI surface:** `nile trim-in|trim-out|split|gap|delete|yank|paste|
  extend|undo|redo`, range flags on delete/yank/paste, `remove --ripple`,
  `--at`/`--clip`/`--seg` addressing, and `nile show` segment indices.
- **IO commands in both:** `:export`/`:detect`/`:edl-import`/`:otio-import`/
  `:add` run in a worker thread (mpsc polled by the event loop) so the
  monitor keeps playing; the printing `cmd_*` functions became `*_work`
  functions returning a `CommandReport` (warnings + summary).
- **Extend routing moved to `nle-core::extend`** (`resolve_armed`,
  `armed_edge`, `plan_commit`, `commit_batch`): the shell's `nile extend
  --edge in|out --to T` commits under the same rules as the TUI's `e`/`E`.
- **Verified:** 284 tests, clippy clean, `./demo.sh` agent chain (split →
  `delete --clip` → `undo` → `gap` → `yank` → `paste` → `export`) with pixel
  checks and a register check.

- **The stored playhead** (`PROJECT_VERSION` 5): `nile seek T` / `:seek T` move
  a shared cursor — snapped to the frame grid, clamped into the timeline; the
  TUI opens there, re-saves its position after every edit, and flushes it
  ~1 s after the transport stops (plus on quit); a reload adopts the file's
  value. `nile show` reports `at N s`. Session state, not a catalog action (no
  history entry; `--at` stays explicit).
- **Shared-session concurrency:** the TUI adopts shell writes within a tick
  (project, history, register, playhead) and never writes over an unseen
  change; reloads wait for Normal mode.
- **Undo covers track and pool edits:** `Change::Tracks` makes add/mute/rm and
  `purge-stumps` undoable, `History::add_asset` makes imports (and an `add`
  that auto-imports) undoable (`HISTORY_VERSION` 10); only whole-project
  rebuilds (`init`, EDL/OTIO/detect) re-baseline.
- **Hardening pass:** an edge-case sweep and then an expected-behaviour sweep
  (hostile input first, then normal workflows asserted on *results*) found 18
  bugs — the CLI's SIGPIPE panic on `| head`, a register that pasted the wrong
  media, a timeline that wrapped negative on absurd input, sub-frame clips
  from `speed`, kind-mismatched exports, a byte-unstable project file, and a
  handful of misleading errors. Both sweeps are in the repo
  (`crates/nle-cli/tests/behaviour.rs`, `tests/hostile.rs`) and run under
  `cargo test`.

**Deferred:** `nile paste --from-clip` (the register covers it); TUI `:probe`.

## M5 — transitions, effects, proxy cache (shipped)

- **Transitions:** a real `Segment::Transition { id, dur, kind, a, b }` that
  **owns** both source sides (`TransitionSource::Clip | Black`), so it never
  dangles when neighbours change and positions stay a prefix sum. Each clip
  side gives up `dur` — clip↔clip shortens the timeline by `dur` (matches
  `xfade`: 10+10−1 = 19), clip↔black leaves it. Export blends with
  `xfade`/`acrossfade`; creation via `X` / `:transition`. Cuts only:
  `Dissolve` + `FadeBlack`. `dd`/`D` remove one and hand the frames it
  trimmed back to its neighbours (only when they still line up); `i`/`o`/`s`/
  `_` refuse. A neighbour's rate change retimes the side it owns — `dur` and
  the cut stay, the inner edge follows — so the seam never steps rate.
- **Effects:** a typed `Effect` enum (`Crop`/`Scale`/`Offset`/`Eq`/`Lut`) on
  `Clip.effects`, applied in the export chain after the retime and before
  the geometry normalize (so layers stay uniform for `overlay`); `:effect`
  creates them. Crop/scale are masks that reveal the tracks below; offset
  moves the picture to or past the frame edge. Undoable (`Atom::Clip`).
  `merge_pair` only joins clips with matching effects; a paste carries them
  over. (The TUI effects panel and one-per-kind editing landed post-M5.)
- **Proxy cache:** segments `edl://` can't express (effects, transitions,
  retimes, an off-sequence picture size — mpv would re-frame that one while the
  export scales it, so the framing would jump at a transition's edge)
  are rendered once into `.nile/<stem>/cache/proxy/<hash>.mp4` and the monitor
  plays the proxy; the hash covers the segment + geometry + frame rate +
  each asset's path **and mtime**. The render puts the segment on its own
  track and reuses the export graph compiler, so there's no second
  filter-graph to drift.
- **Continuous preview audio:** mpv restarts its audio chain when an EDL entry
  switches source files; measured worst-case stalls were ~410 ms at gap/proxy
  edges. When the EDL has a gap or proxy entry, the monitor renders the whole
  timeline's mix once into `cache/audio/<hash>.m4a` (keyed by audio content,
  ignoring split ids/seams), adds it as an external audio track, and lets the
  raw/proxy video EDL switch without pulling the audio clock down. Pure
  raw-cut timelines keep the instant path.
- **Waveform overview:** an active audio track gets a waveform row from cached
  low-rate peaks under `cache/wave`; peaks are keyed by asset path + mtime and
  mapped through clip source spans/speed. Rendering is pure once loaded.
- **Composited preview:** mpv plays one source per EDL span and can't blend or
  mix, so a window where another unmuted track is audible — a second video
  layer under an overlapping clip, or a separate audio track — is rendered as
  the export's own composite (video `overlay`ed, audio `amix`ed) over that
  span, cached, and widened to contain any transition whole (a partial
  crossfade isn't expressible). Elsewhere the monitor stays on raw sources:
  with opaque clips, fall-through *is* `overlay`. Bare `nile edl` prints the
  composite the monitor plays; `--track N` still previews one track.
- **Cache pruning:** each project has its own cache; it only grows (every trim,
  retime, or effect mints a new hash), so `nile cache prune [--days N]
  [--dry-run]` / `:cache prune` deletes that project's entries older than N
  days (default 30), by mtime — which is the
  render time, since a hit does not touch it. Explicit: nothing sweeps on open
  or on a timer; a pruned proxy simply re-renders. Only `<hash>.mp4`,
  `<hash>.mkv`, `<hash>.m4a` (and a crashed render's temp) are candidates, so
  `cache/black/` is never touched. Audio masters live in `cache/audio/` and are
  pruned by the same command.
- **Verified:** unit tests + real ffmpeg runs — a dissolve reads a ≈50%
  blend, `eq` desaturates at `saturation: 0` and darkens with `brightness`,
  and `nile edl` renders and references the proxies.

**Deferred:** thumbnails / waveforms; OTIO transitions; direct `i`/`o`
trimming of a transition's own span (retime it with `x`/`X`,
`--frames`/`--dur`/`--percent` instead).

## M6 — export scheduler, presets, bench (shipped; scheduler opt-in)

- **Presets (shipped):** `preview` = hardware H.264 6M (VideoToolbox on
  macOS, VAAPI on Linux) with CPU fallback;
  `balanced` = libx264 fast CRF 22; `max` = libx265 medium CRF 20.
  `--encoder`/`--crf`/`--encoder-preset`/`--bitrate` override.
- **Bench (shipped):** `nile bench [--runs N] [--preset T] [--parallel N]`
  writes to a temp file and reports wall time/effective fps.
- **Scheduler (shipped, opt-in):** `--parallel N` splits at cuts and
  transition midpoints, renders chunks concurrently, stream-copies the video
  and encodes audio once at the final mux. It uses one encoder unless segment
  durations and source in/out points are frame-aligned (the serial graph
  rounds sub-frame spans, which chunk trims cannot reproduce). Auto stays
  single-worker because local x264/VAAPI benchmarks showed no gain.
- **Resume/cache (shipped):** chunks live in
  `.nile/<stem>/cache/export/<hash>.mkv`; completed chunks survive a failed
  run and are reused. `cache prune` ages both proxies and export chunks.
- **Deferred:** auto-enabling multi-lane scheduling on a workload where a
  local benchmark shows a win; this machine's x264/VAAPI short exports did not.

## Visual selection (shipped)

- `v` selects whole segments on the active track; motions extend the range.
- `d` deletes the selected range as one undo step; `y` copies it into the
  shared yank register; `p` pastes it at the playhead, splitting the clip or
  gap under it when needed; `V` narrows to one segment.
- Range delete restores a boundary transition's surviving handle; blends
  wholly inside the range disappear with it. Deleting a clip or gap beside a
  blend removes the blend too and restores the other side. Paste inside a
  transition is refused.
- `DeleteRange`/`YankRange`/`PasteRange` are catalog actions, so the CLI and
  `:cmd` grammar reach them as `delete|yank --from-seg A --to-seg B` and
  `paste --range`.
- Pasted clips/transitions get fresh ids; edge gaps fold into existing gaps.

## Per-track waveforms (shipped)

- Every unmuted audio-bearing track (including video-embedded audio) gets a
  waveform row under its track; the active track stays brighter.
- Timeline windowing counts each waveform row, so a short panel shows fewer
  whole tracks rather than clipping one.

## Timeline viewport (shipped)

- `+`/`=` zoom in, `-` out, `z` toggles fit / one-frame zoom;
  `Ctrl-b`/`Ctrl-f` pan.
- Zoom is anchored on the playhead and follows seeks, playback and edits.
- Max zoom is one frame per column; fit shows the whole timeline.
- Zoom, scroll, the active track, and pool source marks are saved in
  `.nile/<stem>/view.json` and restored on the next session.

## Pool keyframes (shipped)

- `J`/`K` seek the pool preview to the next/previous video keyframe.
- The probe runs on a worker (packet flags first, keyframe-only frame scan as
  a fallback) and is cached under `.nile/<stem>/cache/keyframes/`, keyed by
  source path + mtime. Audio-only assets and files with no keyframes report
  that instead of seeking.
- The strip marks cached keyframes with `·`; the metadata pane shows the
  count (or a probing state).

## Pool thumbnails (shipped)

- The selected video's metadata pane shows a cached half-block truecolor
  thumbnail, extracted on a worker and letterboxed into the available cells.
- Frames live in `.nile/<stem>/cache/thumbs/<hash>.rgb`, keyed by source path
  + mtime, and are pruned with the other caches.

## Pool source marks (shipped)

- `i`/`o` set source in/out at the preview clock; `I`/`O` seek to them and
  `x` clears them. Marks are frame-snapped, keyed by asset id, and saved in
  `view.json` across restarts.
- `enter` inserts the marked source range; with no marks it inserts the whole
  asset. The strip shows the marks and the metadata pane shows the range.

## Import picker + async import (shipped)

- `a` in the pool opens yazi as a multi-file chooser
  (`--chooser-file`/`--cwd-file`); without yazi it falls back to `:import`.
- `:import PATH|DIR …` parses quoted paths, expands `~`, expands a directory
  to its immediate media files, and probes on a worker with progress.
- A batch import is one undo step; the last new asset is selected in the pool.
- **Deferred:** remember the last import directory across restarts; extend Tab
  completion to the other path-taking commands; add yazi preview/key hints;
  revisit fzf as an alternate picker (yazi is the current one).

## Pre-merge — clipgen engine readiness (next)

`~/clip-generator` (live-show ingest → transcript/CV mining → angle/crop
planning → feedback cases → verification) merges into nile as a **domain
toolkit** on top of the engine, not as a second pipeline. The engine work
lands first so the merge is a port, not an integration.

**Merge-ready means:** one project with several sequences, source↔timeline
time maps and first-class annotations; an analysis job runs through nile and
caches its results; a deliverable renders with a report that maps output
frames back to the edit; all of it is reachable over a revisioned protocol
shared by the TUI and agents — with no clipgen code present.

### 0. Workspace, revisions, fixtures

- Split the binary: `nle-protocol` (request/response/event types + client and
  server, serde only) and keep the TUI in `nle-cli` for now. New domain
  crates later: `nle-analysis`, `nle-director`. Keep the dependency freeze:
  no async runtime, blocking stdio/socket threads suffice.
- `Document` gains a monotonic `revision`; model mutations (edits, asset/track
  changes, undo/redo) bump it, but playhead-only saves do not. History entries
  carry `{ actor, label, at }` provenance. This is the substrate for protocol
  compare-and-swap and human/agent review.
- `fixtures/` + a golden harness: a synthetic 2-cam show (program, mic, ATEM
  switcher log, tiny Resolve project/EDL) with expected project JSON, EDL,
  frame counts and sampled-frame PSNR, wired into `scripts/check.sh`.
- **Order within step 0:** revision + provenance first, then the
  `nle-protocol` skeleton, then fixtures. These are breaking format changes:
  close any live TUI before testing and rebuild old projects.

### 1. Sequences (blocking)

- `Project { media, sequences, active }`; `Sequence { id, name, fps, video,
  tracks, playhead }`. Output settings move onto the sequence so a 16:9 and a
  9:16 deliverable (later a lesson) can coexist; project version bump.
- History is sequence-scoped (`histories: BTreeMap<SequenceId, History>`), so
  undo never mixes sequences. Actions stay unchanged: sequence is session
  scope, resolved like the active track (`--sequence`, TUI switch, protocol
  param).
- `Sequence.origin = Root | Derived { from, spec }` and a manual-edit counter
  land now; derived sequences come later but must be traceable from day one.
- Export takes a sequence; proxy/export caches key per sequence.
- **Gate:** demo + EDL/OTIO builds `sequences[0]`; two sequences with
  different sizes export; the TUI switches and undo is isolated.

### 2. Protocol (blocking)

- NDJSON JSON-RPC over `--stdio` or `.nile/<stem>/session.sock`; methods are
  the action catalog plus reads (`project.show`, `timeline.edl`), jobs,
  sequences and media: `action.preview`, `action.apply` (batched),
  `undo/redo`, `sequence.*`, `media.*`, `job.*`.
- `action.preview` applies a batch to a cloned `Document` and returns the
  diff + warnings; `action.apply` is atomic and returns the new revision.
  A mismatched `base_revision` is a typed conflict with the diff.
- `nile serve` is the single writer when open; one-shot CLI stays file-based;
  the TUI attaches to the server. Events: `project.changed`,
  `sequence.changed`, `job.progress`, `annotation.added`, `playhead`.
- **Gate:** protocol contract tests with golden JSON; the action slot-parity
  test also walks the protocol; two writers get a clean conflict.

### 3. Time maps + media groups (blocking)

- `TimeMap` in `nle-core`: exact rationals, piecewise linear, gaps and
  trimmed spans; `map`, `inverse`, `compose`, `map_range`. Property tests for
  round-trip and boundary exactness.
- `MediaGroup`: the ordered parts of one logical recording (MIC 1 01..N,
  CAM 3 01..N) with per-part offsets; a map resolves group time to
  `(asset, file time)`. This is the multi-part support the importers and
  every analysis consumer need.
- Importers emit maps alongside clips from the start; `nile show --json`
  exposes the mapping so it is exercised, not dead code.
- **Gate:** group → timeline → `(part, seek)` → back is exact; a wall time
  inside a trimmed gap maps to "no media"; a multi-part fixture round-trips.

### 4. Annotations (blocking)

- `Annotation { id, sequence, kind, anchor, payload, provenance, status }`;
  `Anchor = Source { group/asset, time } | Timeline { range }`.
  Kinds: `Note`, `Expectation`, `Directive`, `Marker`, `Chapter`, `PlanSpan`,
  `CropPlan`, `AnglePlan`. Large derived data (transcripts, CV) stays in a
  content-addressed analysis store referenced by range/hash, never in
  `nile.json`.
- Timeline-anchored annotations remap with the same span transform as clips
  on trim/split/ripple/paste, recorded in the undo `Change`; source-anchored
  data survives edits for free.
- Expectations are typed payloads (`cam`, `cropStable`, `workInFrame`,
  `wordsComplete`, `absent`, `noSliver`, …) evaluated against a render
  report. Protocol + TUI add/list/resolve; provenance shows human/model/
  analysis.
- **Gate:** ripple/split/trim/undo move timeline annotations while source
  annotations stay put; a note round-trips through the protocol and is
  visible in the TUI.

### 5. Render: keyframed effects, audio post, reports, jobs (blocking)

- `Effect::Crop` gains keyframes (`x`, `y`, `h`). The renderer groups passes
  by constant `crop_h` (closed rung set), animates position with a filter
  expression, snaps pass bounds to the frame grid and splits at camera cuts;
  proxy hashes include the keys. `demo.sh` gets a pixel check.
- Audio post becomes sequence settings: loudness normalization, speech-safe
  fades, PCM joins with one AAC encode at the final mux (the chunk scheduler
  already has the join discipline; reuse it here).
- `RenderReport { sequence, revision, output, fps, frame_count, frame_map,
  segments, windows, code }` beside the output or under
  `.nile/<stem>/renders/`. The frame map maps output time back to
  sequence/source — the basis for notes on a delivered file.
- Jobs are persistent, observable and cancellable, with progress events and
  retained reports; the TUI job list becomes the real queue.
- **Gate:** a keyframed crop export is frame-exact and its report maps a
  moved note to the right frame; a cancelled job leaves no half-written
  deliverable.

### 6. Analysis sampling + sidecar contract (supporting)

- `nle-ffmpeg` samples frames/audio on demand into packed buffers/files with
  a content-addressed cache (same key discipline as proxies/keyframes:
  path + mtime + version + params). Sidecars consume packed data, not
  per-frame JPEGs.
- Define the sidecar contract (NDJSON requests + packed raw frames) and prove
  it with one trivial analyzer before any ML arrives; ML stays out of process
  and out of the Rust dependency graph.

### 7. Toolkit hooks (after the core)

- Derived sequences (`sequence derive --from … --preset 9x16`), export
  presets per sequence, batch render jobs.
- Marker/chapter annotations in the TUI; transcript and text-based editing
  build on annotations + time maps later.
- `nile mcp` as a thin protocol adapter; a review/log view that shows
  history entries with actor and provenance.
- Import completeness: additive EDL/OTIO import, audio-only EDL, multi-part
  grouping (the ingest foundation for clipgen).

### Non-goals before the merge

No transcripts, CV, LLM calls, selection/crop **policy**, show-folder
assumptions, "social" concepts, course features, titles or nested sequences
in `nle-core`. Those belong to the toolkit on top; keeping the core generic
is what makes the merge modular.

### Order and merge gate

Workspace/revisions/fixtures → sequences → protocol → time maps →
annotations → render (keyframes, audio, reports, jobs) → sampling contract →
toolkit hooks. Do not start the clipgen port until the merge-ready sentence
above is true; then port ingest, planners and cases against the protocol and
prove parity with the existing renders in `fixtures/`.

## Timeline filmstrips (shipped)

- Video tracks get a two-row truecolor strip under the clip row when
  `Ctrl-t` / `:filmstrip` is on (the TUI default; `--dump` stays compact).
- Strips are 64×4 RGB sprites sampled across each clip's source span, cached
  under `.nile/<stem>/cache/filmstrip/` and extracted on a worker. They show
  raw source frames; effects are not baked.
- Extraction decodes the clip's source span; very long spans can be slow.
  Keyframe-only sampling is a possible later optimization.

## Open / deferred

- M7 GUI.
- Tooling: make `demo.sh`/`scripts/check.sh` use a temporary sandbox or refuse
  while a live TUI is attached, so a check cannot wipe a live demo project.

## Open decisions (non-blocking)

1. M7 GUI technology.
2. Per-track sync lock: let an overlay/audio track opt in or out of the base
   track's ripple. Current rule: the first video track is always the master.
