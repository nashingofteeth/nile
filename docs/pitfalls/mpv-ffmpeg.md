# mpv / ffmpeg notes

Read before touching `graph.rs`, `preview.rs`, `proxy.rs`, or the mpv client.

## mpv states

- `--keep-open=always` holds at EOF a frame or two before the probe duration;
  the pool clock snaps to `dur − 1/fps` at EOF.
- Pool preview stops at the reachable source: a clip head previews
  `[0, aout]`, not the whole asset.

## Preview proxy cache

- The monitor proxies whatever `edl://` can't express, rendered by the
  *export* graph (so there is no second filter chain to drift) and cached
  under `.nile/<stem>/cache/proxy`:
  - a *segment* (`preview::needs_proxy`): an effect, a non-1× retime, a
    transition, or a picture size that isn't the sequence's (mpv re-frames a
    raw file to *its* aspect while the export scales it, so the framing would
    jump at the seam); hashed by the segment + geometry + fps + the assets it
    reads;
  - a *span* where another track is audible (`proxy::composite_proxy`): every
    unmuted track's slice of it, hashed the same way over the slices.
- Each asset's **path and mtime** are in the hash, so a replaced file
  re-renders — and so is each **LUT file** an effect reads (the segment JSON
  carries only its path, so editing a `.cube` in place must re-render too).
- The render is `export_quiet`: the monitor calls it, so ffmpeg must never
  write to the TUI's stderr.
- A composite span is widened to contain a transition whole and the slice
  keeps it whole — a partial crossfade isn't expressible in one `edl://`
  entry.
- The hash includes `RENDER_VERSION` (`proxy.rs`): bump it when the *meaning*
  of a segment's JSON changes (a filter rewrite), or stale renders are served.

## Preview audio seams

mpv restarts its audio chain when an `edl://` entry switches to a different
source file. Measured with `--vo=null --ao=null`: raw clip cuts were fine, but
gap/proxy edges stalled the video/audio clock by ~410 ms. The monitor now
renders one continuous mixed audio master to `cache/audio/<hash>.m4a` and adds
it as an external audio file (`audio-add … select`), leaving the EDL's embedded
audio unused. The key ignores clip ids and raw split seams (adjacent
same-asset/speed spans merge), so splitting a clip reuses the master. A
five-second proxy/gap/transition EDL went from
two ~410 ms stalls to none in the same test. The video EDL still switches; it
just no longer drags the audio clock with it.

## Sequence audio post

- **Settings belong to the sequence, the chain to the final mix.**
  `AudioSettings { normalize, fade_in, fade_out }` is applied once after the
  final `concat`/`amix`, so it shapes the programme, not each clip. The
  default (off/zero) appends nothing and keeps the classic graph
  byte-identical; `audio_post_chain` is the one place the filters are spelled.
- **Target is EBU R128 speech-first:** -16 LUFS integrated, -1.5 dBTP true
  peak, LRA 11 (`LOUDNORM_FILTER`). One-pass `loudnorm`; two-pass measurement
  is a later optimization, not a second path. `loudnorm` outputs 192 kHz, so
  the chain pins it back with `aresample=48000` (a fades-only chain never
  changes rate and needs no resample).
- Fades are clamped to the programme length when the chain is built, so
  `afade` never gets a negative `st`. Both are ordinary `afade=t=in|out`; an
  overlap is a product of the two envelopes (no error).
- **No audio stream, no audio post.** A video-only programme is padded with
  silence, and `loudnorm` maps digital silence to NaN (the AAC encoder then
  refuses it), so `has_audio_material` gates the chain: the export still
  carries its AAC silence track but applies no normalize/fade. A *silent*
  audio stream is not detected (that would need decoding); it is the one case
  `loudnorm` can still reject.
- **Chunks defer audio post to the final mux.** A chunk graph renders plain
  PCM with `sub.audio` cleared, and the chunk hash ignores the settings, so a
  loudness/fade change reuses the chunks. `concat` then decodes the joined PCM
  once, passes `audio_post_filter` to `-af` over the whole programme, and
  encodes AAC once — never per chunk and never a double encode.
- The monitor's continuous audio master hashes the settings
  (`AUDIO_PREVIEW_VERSION` bumped), so changing them re-renders the preview
  mix, and `export_audio_quiet` uses the same graph as the deliverable.
  An active setting forces the master even on a clean-cut timeline (no
  seam to hide); `ensure_audio_master` still returns `None` when no source
  has an audio stream.

## Filter-graph gotchas

- `crop`/`scale` are masks: they keep the canvas and pad `black@0` on
  `yuva420p`, so the export overlay (and the monitor's composite, gated by
  `Effect::masks`) reveals the track below; the single-track export drops to
  black. Opaque padding silently breaks "reveal below".
- Crop is a window `{x, y, w, h}` in percentages of the current picture box
  (`x`/`y` top-left, `w`/`h` size), not four independent insets; later
  effects see the cropped box because the chain threads it forward.
- A keyed crop leaves the normal chain: it splits the clip into constant-size
  passes, animates x/y with crop+overlay expressions, and concatenates. It
  must be the only effect on the clip and cannot sit on a transition side.

- Export retimes with `setpts=(PTS-STARTPTS)/speed` + `atempo` (chained
  outside [0.5, 2]); the 1× graph stays byte-identical.
- `overlay`'s `format` option accepts only `yuv420/yuv422/yuv444/rgb/gbrp/
  auto` — `yuva420p` is rejected as invalid, but the overlay's alpha plane
  still drives compositing (the top layer's transparency works anyway).
- A helper that emits a filter chain must push its output label too: the
  multi-track audio pad emitted its `anullsrc…` chain but forgot the label,
  and ffmpeg reported a filter with an unconnected pad.
- A LUT path needs two escapes in this order: option-level (`\`, `:`, `'` each
  take a backslash), then the graph quote, where `'` is `'\''`. Escaping only
  the graph level makes `a:b.cube` fail (exit 234) and `it's.cube` open the
  wrong file. Guards: `a_lut_path_survives_the_option_and_graph_parsers`, and
  the end-to-end `a_lut_path_with_metacharacters_exports`.
