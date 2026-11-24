# Annotations

Read before touching `annotation.rs`, the `Change::Annotations` path, or
`nile annotation`.

- **A source anchor is a media time, not a timeline position.** Trims,
  splits, ripples and paste cannot move it. Do not "helpfully" rewrite a
  source time when a clip moves.
- **A timeline anchor is `{ clip, source }`, not a timeline offset.** Trims,
  speed changes, ripples and paste keep the same footage; a split re-anchors
  to the piece that owns the source; a deleted endpoint drops the annotation
  with a warning. The drop is recorded in the track change, so undo restores
  it. Adding or growing a blend also consumes the source from its neighbour,
  and a blend side has no anchor identity, so a note held in those frames
  drops with the same warning. A point is a frame reference (`[in, out)`):
  trimming away either edge drops it. A range endpoint is closed at the
  clip's tail, so a range can end on a cut: the start belongs to the
  following clip, the end to the preceding clip's tail. Creation supports
  points and ranges (`--to`); `annotation ls` resolves the span.
- **Ids are project-wide, the list is scoped by `sequence`.** `nile
  annotation ls` defaults to the active sequence; `--sequence N` reads
  another. Adding always writes to the active sequence's history, and
  removing/resolving an annotation from another sequence is refused
  (`EditError::AnnotationSequence`) rather than recorded on the wrong stack.
- **`:note` follows the edit scope: the active track's clip.** The monitor
  shows the composite, so an overlay can hide the clip a note anchors to;
  use `:annotation add --asset/--group` for an explicit source.
- **Validation is on every save.** A note needs non-empty text; a source
  time is half-open `[0, duration)` and a group time must resolve through
  `MediaGroup::group_time`. Removing an anchored asset or group is refused
  (`AssetInAnnotation` / `GroupInAnnotation`), never left dangling.
- **One sequence's list is the undo unit.** `AnnotationsChange` carries
  `sequence` and only that sequence's before/after annotations, verified by
  exact equality; another sequence's edits cannot invalidate the stack.
  Resolving an already-resolved annotation records nothing, so undo does not
  step through dead entries.

## Expectations

- **An expectation is a typed payload, not a note.**
  `AnnotationKind::Expectation { expect }` wraps an `Expectation`
  (`nle-core/src/expect.rs`): the check and its tolerances live in the
  variant, so a note cannot grow a tolerance and a check cannot exist without
  one. It takes a **timeline** anchor only — a render span is what gets
  judged — so adding one on a source anchor and loading such a file both fail
  `Project::validate_annotation`.
- **Six checks, one grammar.** `--expect` on the CLI and `:annotation add`
  builds the same `ExpectArgs` through `expect_from_args`; the CLI spelling is
  dashed, the model/wire spelling snake_case.
  - `cam --asset N | --group N` (exactly one) `[--tolerance-frames N]`
    (default 0): the topmost picture is that asset or group member.
  - `crop-stable [--max-move P] [--no-hold-size]`: the crop never moves faster
    than P percentage points per second (default 25) and, unless
    `--no-hold-size`, keeps its size.
  - `work-in-frame [--margin P]`: every crop keeps P% of the picture on all
    four sides (default 5).
  - `words-complete [--min-coverage F] [--transcript HASH]`: without a
    transcript, F of the checked frames carry audio (default 1.0). With a
    `t1-…` hash, F of the relevant words must be whole: a trim, split or span
    edge that cuts a word fails it, and the transcript's asset must be shown
    in the anchored span (otherwise unverifiable). The words live in
    `.nile/<stem>/analysis/transcripts/`; a missing or corrupt transcript is
    unverifiable.
  - `absent [--asset N | --group N] [--black] [--tolerance-frames N]`: none of
    that material appears; `--black` also refuses black frames and may stand
    alone ("nothing black"), otherwise one selector is required.
  - `no-sliver [--min-frames N] [--asset N]`: no visible run is shorter than N
    frames (default 2), optionally only runs showing that asset.
- **A check owns its flags.** `expect_from_args` matches the check name to an
  owned-flag table, so `--black` on `cam` (or `--margin` on `absent`) is
  refused, not ignored. Values are range-checked by
  `Expectation::validation_error` on add and on load, so a hand-edited
  `nile.json` gets the same message as the command line.
- **`crop_stable` is the one deliberate superset.** The CLI's `ValueEnum`
  offers only `crop-stable`; the `:cmd` grammar also accepts the model spelling
  `crop_stable`, because the check names are wire values there.
- **Selectors are uses.** `--asset`/`--group` must name pooled media on add and
  on load, and `nile media rm`/`nile group rm` refuse while an expectation
  names them (`AssetInAnnotation`/`GroupInAnnotation`) instead of leaving a
  dangling id.
- **A check is computed, never stored.** `evaluate` is a pure function of
  `(project, report, annotation)`; `check_with` loads the sidecar and the
  project's transcript store, then picks the annotations; the CLI, protocol
  and TUI share that seam (`cmd/check.rs`). `CheckOutcome` copies the
  annotation's `status`, so a `Resolved` expectation still runs, still fails,
  and prints `[resolved]`: resolving acknowledges a verdict, it does not waive
  it.
- **The sequence defaults to the report's.** An explicit `--sequence` that
  names another sequence is exit 2; an `--id` that selects no expectation is
  also 2 (a stale filter must not pass a gate silently). A crop,
  work-in-frame, word or group check against a report whose revision differs
  from the project's is unverifiable ("re-export") — the report carries no
  effects or probes.
- **The protocol path is absolute.** `annotation.check` expands `~`, then
  refuses a relative path: the server's cwd is not the client's. The CLI and
  TUI read local paths directly.
- **TUI verdicts are session state.** `:annotation check --report FILE
  [--sequence N] [--id N]` logs the `format_human` lines and reuses the last
  (the summary) in the message line; the marks live in `App::checks` keyed by
  the report's sequence, cleared on reload and on any edit that changes that
  sequence. The marker row draws `✓`/`✗`/`?` at the span start through
  `Viewport::col`; the most severe wins (fail > unverifiable > pass).

## Transcripts

- **Content, not a name.** `Transcript` is one asset's word list in source
  seconds; `Transcript::hash` canonicalises the words and hashes the JSON to
  `t1-…`. The store is `.nile/<stem>/analysis/transcripts/<hash>`; re-importing
  changed words makes a new address, while an old hash keeps naming exactly
  the old words (no silent substitution).
- **Import.** `nile analysis import --asset N FILE` reads
  `{ "words": [ { "text", "start", "end" } ] }` (exact rational source
  seconds), checks the asset and its probed duration, stores the canonical
  document atomically, and prints the hash. `analysis.add { asset, words }`
  is the protocol form and returns `{ hash }`; it writes analysis state, not
  the project (no revision, history or event).
- **Check.** `words-complete --transcript HASH` requires each relevant word to
  be whole in one shown clip/transition side (trim, split or anchor-edge cuts
  fail), with the transcript asset actually shown in the anchored span. A
  point anchor, a missing/corrupt transcript, an absent/silent asset, or a
  transcript asset outside the span is unverifiable. Without a hash, the same
  check is audio coverage.
