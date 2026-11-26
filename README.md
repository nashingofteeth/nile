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
- `clang` — Xcode Command Line Tools on macOS; Linux links through `lld`
  (see `.cargo/config.toml`)
- **ffmpeg/ffprobe 9+** — the export passes its filter graph through
  `-/filter_complex`, which older ffmpeg (including Ubuntu 24.04's 6.x) lacks
- `mpv` for the interactive monitor

macOS:

```sh
xcode-select --install                 # clang
brew install ffmpeg mpv yazi shellcheck
```

No linker setup beyond Xcode: macOS uses Apple's `ld64`, while Linux asks
clang for `lld`.

## Build and install

```sh
cargo build --release
./target/release/nile --help

# Install `nile` and create the `nle` alias symlink
./scripts/install.sh

# Headless CLI only (no TUI/mpv; cargo feature `tui` off)
cargo build --release --no-default-features
```

If `nile` is not found after installing, `cargo install` put it in
`$CARGO_HOME/bin` (default `~/.cargo/bin`); add that directory to `PATH`.

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

Run `nile edit`. If `nile serve` is live for this project, the TUI attaches to
its socket and sends edits through the protocol; without a server it opens the
file directly. Attached mode keeps the server as the single writer: agent
edits arrive on the next poll, `:import` uses `media.add`, and
`:add`/`:detect`/`:edl-import`/`:otio-import` are refused until the server is
stopped. The timeline panel shows one row per track; navigation and edits
follow the **active track**, while the monitor plays the composite of every
unmuted track. The keymap is vim-flavored rather
than a vim clone: motions and pending operators use vim vocabulary, while
edits keep NLE mnemonics (`i`/`o` are trim in/out; `s`/`S` split).

Any length-changing edit on the **first video track** — ripple insert/delete,
ripple trim, speed, gap insert, extend, transition retime — shifts or removes
the same span on every other track, so separate audio/overlay tracks stay in
sync. Edits on an overlay or audio track stay local, so layering a clip at 0
does not move the base. If the span crosses a transition on another track,
that track stays local and the message says sync was skipped. There is no
per-track sync-lock toggle yet: the first video track is always the master.

Video tracks show a two-row filmstrip under the clip labels: raw source
frames sampled across each clip's source span and cached under
`.nile/<stem>/cache/filmstrip/`. `Ctrl-t` or `:filmstrip` toggles the rows
for this session; effects are not baked into the preview strip.

A project can hold several **sequences** — separate timelines with their own
frame rate, output size, playhead, audio post and undo stack, sharing one
media pool. The TUI edits the active sequence; the status row names it when
there is more than one. `:sequence ls` lists them, `:sequence N` switches, and
`:sequence add NAME [--width W --height H --fps RATE]` creates one. Switching
saves `active`, adopts that timeline's playhead, resets the active track and
reloads the monitor. Undo is per sequence: an edit made on one timeline never
replays on another.

Audio post is per sequence and off by default: `nile sequence audio
[--normalize [BOOL]] [--fade-in S] [--fade-out S]` normalizes the mixed
programme to -16 LUFS / -1.5 dBTP (EBU R128, speech-first) and adds
programme-head/tail fades. The same line works in the TUI without `nile`
(`:sequence audio …`), and the protocol exposes it as `sequence.audio`. It is
undoable like an edit; with no flags the command prints the current setting.
Fades longer than the timeline are clamped at export.

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
| `Ctrl-t` | Toggle timeline filmstrips |
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
video keyframe (probed on a worker and cached under `.nile/<stem>/cache/`;
the strip marks them with `·`). The metadata pane shows a cached half-block
thumbnail of the selected video. `i`/`o` set the source in/out at the preview
clock, `I`/`O` jump to them, `x` clears them, and `enter` inserts that source
range at the timeline cursor (whole asset when unmarked). `d` removes the
asset; `u` undoes; `:` opens the command line.

`a` opens the **import picker**: yazi starts in the selected asset's folder
(or the project folder), `space` selects files/directories, and `enter`
imports them. Yazi is optional — without it, `a` opens the pre-filled
`:import` line, where Tab completes paths (spaces are quoted for you).
Importing probes on a worker and adds the whole batch as one undo step.
While the pool is open, the timeline row and monitor show the selected asset;
reopening returns to the same asset and preview clock.

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
:sequence [ls|N|use N]         :sequence add NAME [--width W --height H --fps RATE]
:sequence audio [--normalize [BOOL]] [--fade-in S --fade-out S]
:media ls|rm                  :cache prune [--days N] [--dry-run]
:filmstrip [on|off]           # timeline strips (session-only)
:import PATH|DIR …            :add FILE
# Tab completes paths; quote paths with spaces, or use the yazi picker (`a`)
:show / :seek T
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
- Crop is a window: `x`/`y` are the top-left and `w`/`h` the size, as
  percentages of the picture at that point (`crop` after `scale` uses the
  scaled picture). Scale is a percentage of the same box (100 = unchanged);
  scaling down pads transparently. Offset moves a percentage of the way to
  the edge (100 = fully off-screen). Effects compose in the order added.
- LUT effects are CLI-only in the TUI for now: `:effect lut --path FILE`.

## CLI

Every command has `--help`. Times accept seconds (`12.5`) or timecodes
(`0:01:23.500`).

### Project and media

```sh
nile init
nile import intro.mp4 b-roll.mov
nile import footage/                      # immediate media files, one undo step
nile import --group "CAM 3" part1.mp4 part2.mp4
# one logical recording from ordered parts; one undo removes both
nile group ls                             # part count and group end
nile group rm 1                           # undoable; assets stay pooled
nile import-edl show.edl --media . --group "CAM 3=cam3_a.mp4,cam3_b.mp4"
nile import-otio show.otio --group "CAM 3=cam3_a.mp4,cam3_b.mp4"
# explicit parts join one recording (argument order; repeatable flag)
nile import "clip one.mp4" 'b roll.mov'   # quoted paths (the shell eats these)
nile add intro.mp4 --in 2.5 --out 7       # auto-imports a new file
nile add b-roll.mov --at 10 --track 2     # tracks are 1-based
nile track add video                      # overlay track
nile track add audio                      # mixed on export
nile track ls
nile sequence ls                          # timelines, active marked with *
nile sequence add Vertical --width 1080 --height 1920 --fps 30
nile sequence use 2                       # edits, undo and export follow it
nile sequence audio --normalize           # -16 LUFS / -1.5 dBTP on the mix
nile sequence audio --fade-in 0.5 --fade-out 1  # undoable programme fades
nile show                                 # or --json (the persisted project)
nile show --json --maps                   # add derived time and group maps
nile show --sequence 1                    # report another timeline

# Annotations: source notes are edit-proof; timeline notes follow clips
nile annotation add "check the cut" --asset 1 --time 1.5
nile annotation add "mic check" --group 2 --time 3  # group-clock time
nile annotation add "look here" --timeline --at 2.5 # clip-relative point
nile annotation add "this take" --timeline --at 2 --to 5 # range
# CHECK = cam|crop-stable|work-in-frame|words-complete|absent|no-sliver
nile annotation add --expect cam --asset 1 --timeline --at 2 --to 5
nile annotation add --expect crop-stable --max-move 10 --timeline --at 2 --track 2
nile annotation check --report final.mp4   # video or .report.json
nile annotation check --report final.mp4 --sequence 2 --id 3 --json
# exit 0 pass · 1 fail or unverifiable · 2 error
nile analysis import --asset 1 take.json    # prints the t1-… transcript hash
nile analysis analyze --asset 1 --duration 1 --fps 5 --width 32 --height 24
# samples the source window, runs the bundled mean-luma analyzer as a child
# process, and prints the a1-… product hash (stored under analysis/products/)
nile annotation add --expect words-complete --timeline --at 2 --to 5 --transcript t1-…
nile annotation ls                        # id, status, anchor and text
nile annotation resolve 1                 # undoable; rm deletes instead
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
nile effect crop --clip 3 --x 10 --y 5 --w 80 --h 90
nile effect crop --clip 3 --at 2 --x 50 --y 0 --w 50 --h 75
# a crop keyframe: x/y move between keys, w/h step at keys
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
nile export --sequence 2 -o vertical.mp4   # render a non-active timeline too
nile export -o fast.mp4 --preset preview   # hardware 6M: VideoToolbox/VAAPI
nile export -o master.mp4 --preset max     # x265 medium CRF 20
nile report final.mp4                      # frame map written beside the export
nile report final.mp4 --json               # the full report sidecar
# Preview uses VideoToolbox on macOS and the first /dev/dri/renderD* on
# Linux (override with NLE_VAAPI_DEVICE); a hardware failure retries on CPU.
# Explicit overrides: --encoder cpu|vaapi|qsv|videotoolbox|x265|svt-av1,
# --crf N, --encoder-preset P, --bitrate B.

nile export -o final.mp4 --parallel 2      # chunked export, opt-in
# Chunks split at cut points and transition midpoints, render independently,
# stream-copy the video and encode the audio once at the final mux. Cached in
# .nile/<stem>/cache/export; a repeat export reuses completed chunks. It falls
# back to one encoder unless segment durations and source cuts are on the frame
# grid. If a hardware chunk fails, the whole run retries on the CPU rather than
# mixing encoder heads in one stream-copied file. Auto mode stays single-worker:
# local tests showed no gain.
# A sequence with audio post (normalize/fades) also stays single: those filters
# shape the whole programme, so chunks would repeat them at every seam.

nile bench [--runs N] [--preset preview|balanced|max] [--parallel N]
# Renders to a temp file and reports wall time and effective fps.
```

### Durable jobs (needs `nile serve`)

```sh
nile job submit -o final.mp4 [--sequence N] [--preset balanced] [--crf 22]
nile job ls                                # id, state, progress, output
nile job show 1                            # the full record as JSON
nile job cancel 1                          # drop queued or kill the render
nile job rm 1                              # delete a terminal record + snapshot
```

`nile export` stays synchronous and file-based. `nile job submit` instead
snapshots the server's current project and queues it: the render keeps working
after the shell exits, survives a server restart when still queued, and never
silently resumes a job that was mid-render. One export runs at a time, oldest
first. A finished job keeps its report beside the output
(`final.mp4.report.json`) and links it from the record. The TUI's `:jobs`
panel is the same queue; `x` cancels, `o` points at the report.

State lives under `.nile/<stem>/jobs/<id>/` as `job.json` (the record) plus
`project.json` (the submit-time snapshot). `nile job rm` removes both for a
terminal job; queued or running jobs must be cancelled first.

### One-shot tools and logging

```sh
nile probe clip.mp4 [--json]
nile trim clip.mp4 --in 2.5 --out 7 -o cut.mp4 [--encoder cpu|vaapi|qsv|videotoolbox] [--copy]
nile edl                                  # composite URL the monitor plays
nile edl --track N                        # one track (debugging)

NLE_LOG=/tmp/nle.log nile edit            # timestamped diagnostics
```

### Protocol server (experimental)

```sh
nile serve                                # .nile/<stem>/session.sock
nile serve --stdio                        # pipe mode for scripts/tests
```

`nile serve` exposes the loaded project over JSON-RPC 2.0, one object per
line, on the project's Unix socket (or stdin/stdout with `--stdio`). One
server per project owns the socket; a second `nile serve` refuses. Multiple
connections serialize their calls through one handler. Call `hello` first,
then `project.show`, `timeline.edl`,
`sequence.list|use|add|audio`, `media.list|add`, `action.preview`,
`action.apply`, `undo`, `redo`, or the durable `job.submit|list|get|cancel`.
Actions apply to the active sequence; add media and a second sequence first,
then switch to it:

```json
{"jsonrpc":"2.0","id":1,"method":"hello"}
{"jsonrpc":"2.0","id":2,"method":"media.add","params":{"base_revision":4,"paths":["footage/take2.mp4"]}}
{"jsonrpc":"2.0","id":3,"method":"sequence.add","params":{"base_revision":5,"name":"Vertical","fps":"30/1","video":{"width":1080,"height":1920}}}
{"jsonrpc":"2.0","id":4,"method":"sequence.use","params":{"id":2}}
{"jsonrpc":"2.0","id":5,"method":"action.apply","params":{"base_revision":6,"actions":[{"action":"split","track":0,"at":1.0}],"from":"1/2"}}
```

`action.preview` additionally returns a structural `diff` and clamp
`warnings`; `action.apply` returns the plain outcome. The wire is pre-1.0 and
may change. In socket mode one server is the single writer for the project;
one-shot CLI edits are still file-based and should not run alongside it.
Piping requests into bare `nile serve` now blocks waiting for the socket; add
`--stdio` for a pipe. In `--stdio` mode `stdout` is protocol-only; logs go to
stderr or `NLE_LOG` in both modes. The full contract, including the error
codes and the event order, is in
[docs/pitfalls/protocol.md](docs/pitfalls/protocol.md).

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
        ├── session.sock   # live serve socket (present while serving)
        ├── serve.lock     # startup single-writer lock
        ├── clipboard.json
        ├── view.json      # zoom, scroll, active track, pool source marks
        ├── analysis/
        │   ├── transcripts/  # content-addressed transcript words (t1-…)
        │   └── products/     # analyzer outputs (a1-…)
        ├── jobs/          # durable render jobs (id/job.json + snapshot)
        └── cache/
            ├── analysis/      # packed frame/audio samples (disposable)
            ├── proxy/         # rendered video proxies
            ├── audio/         # continuous preview audio masters
            ├── wave/          # cached audio peaks for the waveform
            ├── keyframes/     # cached video keyframe timestamps
            ├── thumbs/        # cached pool thumbnails (RGB)
            ├── filmstrip/     # cached timeline filmstrip sprites (RGB)
            ├── export/
            └── black/
```

Multiple projects in one directory get separate `.nile/<stem>/` folders.
Legacy adjacent `*.history.json` / `*.clipboard.json` files are copied into
the state directory on first load; old `nle.json` projects still open with
`--project nle.json`. The timeline viewport, active track, and pool source
marks are saved in `view.json` and restored on the next session. The old global
`~/.cache/nile` is no longer used, and `nile cache prune` only touches the current project’s cache — proxy
renders, preview audio masters, waveform peaks, keyframe lists, asset
thumbnails, completed export chunks, and packed analysis samples plus abandoned
analysis staging directories (never `analysis/products/`).

The TUI watches the project, history, and register; a shell edit or seek is
adopted within a tick and is never overwritten unseen. Reloads wait while a
mode holds indices (extend, pool, `:` line), then apply in Normal mode.

## One action catalog, four front-ends

Every timeline edit is one `Action` in `nle-core`, executed by one runner:

| Front-end | For | Example |
|---|---|---|
| TUI keys | frequent actions | `s`, `dd`, `p`, `e`/`E`, `u` |
| TUI `:` line | every action | `:delete --at 3 --ripple` |
| CLI | every action | `nile delete --at 3 --ripple` |
| Protocol server | agents and scripts | `action.apply` with `{"actions":[{"action":"split",…}]}` |

Edits made in any front-end share undo history, the playhead, and the yank
register. Commands that need processes (`:import`, `:export`, `:detect`,
`:edl-import`, `:otio-import`, `:add`) run in a worker while the monitor keeps
playing.

## Compatibility

Early software: project and history formats may change. Old files are rejected
with a clear error rather than silently misread; rebuild them with
`./demo.sh`. Project files are local state, not a distributed interchange
format.

## License

MIT — see [`LICENSE`](LICENSE).
