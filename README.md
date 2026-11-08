# nile

a terminal-native NLE

A TUI dashboard, an mpv monitor, vim-style keys, and FFmpeg underneath. The
Rust process stays light while ffmpeg and mpv do the decoding, rendering, and
playback.

The project and command are **nile**; **`nle` remains as a symlink alias**.

## Features

- **A real timeline in the terminal** — trim, split, gap, delete, yank/paste,
  multi-track video and audio, transitions, and per-clip effects.
- **Vim-style editing** — counts, cuts/gaps navigation, extend mode, undo/redo,
  a command line, and a media pool with live file preview.
- **Fast previews** — mpv plays the composite; effects, speed changes,
  transitions, and mixed/overlaid spans are rendered once into a proxy cache.
- **One action catalog** — TUI keys, the `:` line, and the CLI share one model,
  one undo history, one playhead, and one yank register.
- **Interoperability** — CMX3600 EDL and OpenTimelineIO import, cut detection
  from a rendered file, frame-exact export.
- **Small dependency set** — the Rust side is a single native binary; the heavy
  media work is delegated to ffmpeg/ffprobe/mpv.

Roadmap and milestone history: [PLAN.md](PLAN.md).

## Requirements

- Rust with edition 2024 support
- `clang` and `lld` for linking (see `.cargo/config.toml`)
- **ffmpeg/ffprobe 9+** — the export passes its filter graph through
  `-/filter_complex`, which older ffmpeg (including Ubuntu 24.04's 6.x) lacks
- `mpv` for the interactive monitor

## Build and install

```sh
cargo build --release
./target/release/nile --help

# Install `nile` and create the `nle` alias symlink
./scripts/install.sh

# Headless CLI only (no TUI/mpv; cargo feature `tui` off)
cargo build --release --no-default-features
```

## Quick start

```sh
nile init
nile import intro.mp4 b-roll.mov
nile add intro.mp4 --in 2.5 --out 7      # trimmed clip at the end of track 1
nile add b-roll.mov --at 10              # creates a gap before it
nile edit                                # TUI + mpv monitor
nile export -o final.mp4

# Pick another default for this directory (stored in .nile/config.json)
nile default trip.json
nile show                    # now uses trip.json
nile default --unset         # back to nile.json

# One command can override the directory default
nile -p other.json show
```

The project lives in `nile.json` (or the directory default); edits are saved
automatically. Shell commands and an open TUI adopt each other's changes.
`--project` overrides `.nile/config.json`.

## Demo

`./demo.sh` builds the single canonical demo project,
`demo-sandbox/nile.json`, and exports `demo-sandbox/demo.mp4`. It exercises the
current feature set: multi-track video/audio, waveform rows, a transition,
crop/scale/eq/offset effects, speed, the pool, shell↔TUI hand-off, and the TUI’s
visual-mode and viewport diagnostics. A longer tail keeps zoom/pan meaningful.

Media generation is cached by `demo-sandbox/.demo-media`. Regression fixtures
run under `demo-sandbox/checks/` and are removed on success, so the sandbox
always ends with one demo project. Add new feature coverage there rather than
creating another demo project or another top-level export. Maintenance notes:
[docs/demo.md](docs/demo.md).

## TUI

Run `nile edit`. The timeline panel shows one row per track; navigation, edits,
and the monitor follow the **active track**. The keymap is vim-flavored rather
than a vim clone: motions and pending operators use vim vocabulary, while
edits keep NLE mnemonics (`i`/`o` are trim in/out; `s`/`S` split).

### Transport and motion

| Key | Action |
|---|---|
| `space` | Play/pause |
| `h` / `l` | Seek −1 s / +1 s |
| `H` / `L` | Seek −5 s / +5 s |
| `f` / `F` | Step one frame |
| `j` / `k` | Next/previous cut |
| `w` / `b` | Next/previous clip start |
| `n` / `N` | Next/previous gap |
| `gg` / `G` / `0` | Start / end / start |
| `<` `>` (`,` `.`) | Clip start / clip end |
| `+` / `=` / `-` | Zoom timeline in / in / out |
| `z` | Toggle fit / closest zoom |
| `Ctrl-b` / `Ctrl-f` | Pan timeline left / right |
| `3l`, `5j`, … | Vim counts: seek 3 s, jump 5 cuts |

Zoom is centred on the playhead. Maximum zoom-in is one frame per column;
`z` toggles between that close-up and fitting the whole timeline. Seeking and
edits keep the playhead in view while zoomed; `Ctrl-b`/`Ctrl-f` pan by half a
screen.

### Editing

| Key | Action |
|---|---|
| `i` / `o` | Trim in/out; uppercase (`I`/`O`) ripples |
| `s` / `S` | Split at the playhead |
| `v` | Visual segment selection |
| `_` | Insert a gap (count = frames) |
| `dd` / `D` | Delete (leave a gap) / delete and ripple |
| `yy` | Yank clip or gap into the shared register |
| `p` / `P` | Paste-over / ripple-insert |
| `u` / `Ctrl-r` | Undo / redo |
| `t` / `T`, `<N>t` | Cycle tracks / jump to track N |

### Effects, transitions, and the pool

| Key | Action |
|---|---|
| `x` | Effects panel for the clip under the playhead |
| `X` / `xx` | Cross-dissolve at the boundary (count = frames) |
| `x` / `X` on a blend | Open its length form |
| `r` on a blend | `:transition --percent` (50 = twice as long) |
| `m` | Open/close the media pool |
| `e` / `E` | Enter extend mode (replace / ripple) |
| `[` / `]` | Enter extend mode on the right/left side of a cut |
| `r` | Speed command prompt (100 = normal) |
| `R` | Reload the project from disk |

**Visual mode.** `v` selects whole segments on the active track; motions
(`h/l`, `j/k`, `w/b`, `n/N`, `gg/G`, counts) extend the range. `d` deletes the
selected range as one undo step, `y` copies it into the shared yank register,
and `p` pastes the whole range at the playhead (splitting the clip or gap under
it when needed). `V` narrows the selection to one segment; `v` or `Esc`
cancels. A range may include a transition: its surviving outer handle is
restored, while a blend wholly inside the range disappears with it. The same
operations are actions, so the CLI expresses them as
`delete --from-seg A --to-seg B`, `yank --from-seg A --to-seg B`, and
`paste --range`.

**Effects panel.** `j`/`k` select, `1`–`9` edit that row, `Enter` edits, `d`
drops, `D` clears. Add crop/scale/eq/offset with `c`/`s`/`e`/`o`; LUTs are
`:effect lut` only for now. A blank field keeps the applied value (shown as
“was N”); a letter jumps to its field, Tab/↑↓ move, Enter applies, Ctrl-U
clears, Esc cancels. In scale, `a` or the aspect-lock row toggles locking
(default on, remembered). One effect per kind: adding a kind again edits it.

**Transition form.** Inside a blend, `x` or `X` opens the frames field;
typing a count pre-fills it. `r` opens the rate command. A transition is part
of both neighbours, so trimming/extending nearby moves the blend’s edge.

**Media pool.** `j`/`k` (and `{`/`}`) move; `space` plays/restarts the asset;
`h`/`l`, `f`/`F`, `gg`/`0`, `G`/`$` seek; `J`/`K` jump to the next/previous
video keyframe (probed on a worker and cached under `.nile/<stem>/cache/`);
`enter` inserts at the timeline cursor and returns; `d` removes the asset;
`i` imports; `u` undoes. While the pool is open, the timeline row and monitor
show the selected asset; reopening returns to the same asset and preview
clock.

### Extend mode

`e`/`E` arms the boundary under the playhead — a gap on the right is preferred.
`[`/`]` arm the right/left side of a cut; either bracket at a transition arms
the neighbour’s real edge. Navigate to the target (the monitor shows the armed
material), then `e` commits (replaces what it crosses) or `E` inserts space.
`<`/`>` switch ends, `[`/`]` switch sides, Esc cancels.

### Command line

The `:` line is the CLI grammar, flag for flag:

```text
:trim-in|out [--ripple]       :split
:gap [--frames N|--dur T]     :delete|:yank [--at T|--clip N|--seg N]
:delete|:yank --from-seg A --to-seg B
:paste [--ripple]             :paste --range [--at T]
:extend --edge in|out --to T [--ripple]
:speed PERCENT                :transition [dissolve|fade] [--frames N|--dur T|--percent P]
:effect [--clip N] [--index N] crop|scale|offset|eq|lut …
:effect rm [--index N]        :effect clear
:undo / :redo                 :purge
:track add video|audio        :track mute [N] / :track rm N
:media ls|rm                  :cache prune [--days N] [--dry-run]
:import / :add                :show / :seek T
:export -o FILE [--preset T]  :detect FILE
#        [--parallel N] (chunked export; opt-in)
:edl-import / :otio-import    :help / :q
```

`--at` defaults to the playhead, `--track` to the active track, and `--clip`
to the clip under the playhead. `:help` lists the current mode’s keys.

### Status row

The crocodile reacts to edits: chomp delete, closed-mouth gulp insert, spit
undo and reverse-spit redo, snap split, nibble trim, dash speed, wink effects,
slither transitions, and hiss errors. It blinks when idle and swims while
playing; pausing returns it to idle, and muting closes its eyes.

While the monitor rebuilds, the status row shows a spinner and
`proxies 2/5 · 43%`; the monitor keeps the previous frame until the new proxy
is ready.

### Diagnostics

```sh
nile edit --dump [--dump-size 100x30] [--dump-keys "llx"]
```

Renders the TUI once without mpv, feeds the keys (`\n` = Enter, `\e` = Esc),
and prints the model state plus a grid with a column ruler. Read-only: it runs
on a copy and never changes the project.

## Editing rules

- Tracks are 1-based. `--track N` addresses position-based targets; `--clip N`
  finds the clip’s own track.
- Trimming at a segment’s edge deletes the segment that edge would consume:
  `i`/`I` removes the segment ending at the playhead, `o`/`O` the one starting
  there. This is deliberate.
- A trim beside a transition moves the blend’s near edge, keeping the timeline
  length. Inside a blend, `i`/`o` trim the neighbouring clip’s portion. A
  transition’s own edge is not a neighbour’s edge.
- A blend is opaque, video-only, and part of both neighbours. A clip↔clip
  transition shortens the timeline by its duration; a fade to black does not.
  Retiming (`--frames`, `--dur`, `--percent`) scales each side’s span, never
  its rate. Details: `docs/pitfalls/transitions.md`.
- Deleting a clip or gap beside a blend removes the blend too: `dd` leaves the
  target’s visible span as a gap, `D` ripples it away, and the other side’s
  borrowed length is restored.
- Crop insets percentages toward the opposite edge (0 = edge, 100 = opposite;
  the two sides of an axis must total under 100). Scale is a percentage of the
  picture at that point (100 = unchanged); scaling down pads transparently.
  Offset moves a percentage of the way to the edge (100 = fully off-screen).
  Effects compose in the order added; crop resolves after scale.
- LUT effects are CLI-only in the TUI for now: `:effect lut --path FILE`.

## CLI

Every command has `--help`. Times accept seconds (`12.5`) or timecodes
(`0:01:23.500`).

### Project and media

```sh
nile init
nile import intro.mp4 b-roll.mov
nile add intro.mp4 --in 2.5 --out 7       # auto-imports a new file
nile add b-roll.mov --at 10 --track 2     # tracks are 1-based
nile track add video                      # overlay track
nile track add audio                      # mixed on export
nile track ls
nile show                                 # or --json
nile remove 2                             # by clip id
nile media ls
nile media rm 3                           # only unused assets
nile seek 12.5                            # shared playhead
```

### Timeline edits

```sh
nile split --at 3
nile trim-in --at 2 --ripple
nile trim-out --at 6
nile gap --at 4 --frames 30
nile delete --at 5
nile delete --clip 3 --ripple
nile delete --seg 2
nile yank --clip 3
nile paste --at 5 --ripple
nile delete --from-seg 0 --to-seg 1
nile yank --from-seg 0 --to-seg 1
nile paste --range --at 5
nile extend --clip 3 --edge out --to 8
nile speed 3 50                           # clip #3 at 50%
nile transition --at 3 --frames 20
nile undo / nile redo
nile purge-stumps
```

`--at` is the playhead in the TUI; `--to` may be negative (a gap head reaching
back past time 0). `nile show` prints `[i]` segment indices and `#n` clip ids.

### Effects

```sh
nile effect crop --clip 3 --left 10 --right 10 --top 5 --bottom 5
nile effect scale --clip 3 --w 50 --h 50
nile effect offset --clip 3 --x 10 --y -5
nile effect eq --clip 3 --brightness -0.2  # contrast/saturation default to 1
nile effect eq --clip 3 --index 0          # replace row 0 (0-based)
nile effect rm --clip 3 [--index N]
nile effect clear --clip 3
```

Effects are rendered at export; the monitor plays a cached proxy for clips
with effects, speed changes, transitions, or a picture size that differs from
the sequence. Every unmuted audio-bearing track also gets a cached peak
waveform row directly beneath it (the active track is brighter).
When the EDL has gaps or proxy entries, the preview also
renders one continuous audio master; mpv plays that as an external audio
track, so an EDL source switch does not restart the audio chain and stall the
picture.

### Import, detect, and export

```sh
nile import-edl sequence.edl --media ~/footage
# Cuts only: dissolves/wipes/keys become cuts (each reported); black becomes
# a gap; zero-length events are skipped. --fps/--src-offset/--map/--force.
nile import-otio sequence.otio
# OpenTimelineIO e.g. a Kdenlive export; media paths come from the file;
# --media/--remap/--fps. Kdenlive timewarps import as clip speed.
nile detect-edits edit.mov [--list]
nile detect-edits edit.mov --append --track 2
# One clip per detected shot; black spans become gaps. Tune with
# --threshold/--min-shot/--black-min/--no-gaps/--video-only/--fps/--force.
nile export -o final.mp4                   # balanced: x264 fast CRF 22
nile export -o fast.mp4 --preset preview   # VAAPI 6M; CPU fallback if unavailable
nile export -o master.mp4 --preset max     # x265 medium CRF 20
# Preview uses the first /dev/dri/renderD*; override with NLE_VAAPI_DEVICE.
# Explicit overrides: --encoder cpu|vaapi|qsv|x265|svt-av1, --crf N,
# --encoder-preset P, --bitrate B.

nile export -o final.mp4 --parallel 2      # chunked export, opt-in
# Chunks split at cut points and transition midpoints, render independently,
# stream-copy the video and encode the audio once at the final mux. Cached in
# .nile/<stem>/cache/export; a repeat export reuses completed chunks. It falls
# back to one encoder unless segment durations and source cuts are on the frame
# grid. If a hardware chunk fails, the whole run retries on the CPU rather than
# mixing encoder heads in one stream-copied file. Auto mode stays single-worker:
# local tests showed no gain.

nile bench [--runs N] [--preset preview|balanced|max] [--parallel N]
# Renders to a temp file and reports wall time and effective fps.
```

### One-shot tools and logging

```sh
nile probe clip.mp4 [--json]
nile trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder vaapi] [--copy]
nile edl                                  # composite URL the monitor plays
nile edl --track N                        # one track (debugging)

NLE_LOG=/tmp/nle.log nile edit            # timestamped diagnostics
```

## Files and concurrency

The project is `nile.json` unless this directory sets another default. The
directory setting lives at `.nile/config.json` (written by `nile default`);
everything else is grouped beside the project by stem:

```text
video/
├── nile.json
└── .nile/
    ├── config.json    # optional default_project for this directory
    └── nile/
        ├── history.json
        ├── clipboard.json
        ├── view.json      # timeline zoom, scroll, and active track
        └── cache/
            ├── proxy/         # rendered video proxies
            ├── audio/         # continuous preview audio masters
            ├── wave/          # cached audio peaks for the waveform
            ├── keyframes/     # cached video keyframe timestamps
            ├── export/
            └── black/
```

Multiple projects in one directory get separate `.nile/<stem>/` folders.
Legacy adjacent `*.history.json` / `*.clipboard.json` files are copied into
the state directory on first load; old `nle.json` projects still open with
`--project nle.json`. The timeline viewport and active track are saved in
`view.json` and restored on the next session. The old global
`~/.cache/nile` is no longer used, and `nile cache prune` only touches the current project’s cache — proxy
renders, preview audio masters, waveform peaks, keyframe lists, and completed
export chunks.

The TUI watches the project, history, and register; a shell edit or seek is
adopted within a tick and is never overwritten unseen. Reloads wait while a
mode holds indices (extend, pool, `:` line), then apply in Normal mode.

## One action catalog, three front-ends

Every timeline edit is one `Action` in `nle-core`, executed by one runner:

| Front-end | For | Example |
|---|---|---|
| TUI keys | frequent actions | `s`, `dd`, `p`, `e`/`E`, `u` |
| TUI `:` line | every action | `:delete --at 3 --ripple` |
| CLI | every action | `nile delete --at 3 --ripple` |

Edits made in either front-end share undo history, the playhead, and the yank
register. Commands that need processes (`:export`, `:detect`, `:edl-import`,
`:otio-import`, `:add`) run in a worker while the monitor keeps playing.

## Compatibility

Early software: project and history formats may change. Old files are rejected
with a clear error rather than silently misread; rebuild them with
`./demo.sh`. Project files are local state, not a distributed interchange
format.

## License

MIT — see [`LICENSE`](LICENSE).
