# Crop keyframes

Read before touching `CropKey`, the keyframe action, or the keyed-crop graph.

- **`at` is clip-relative timeline seconds after speed**, not source time and
  not sequence time. The action takes `--at` in sequence time and checks it
  is inside the clip; a key outside returns `CropKeyOutside`.
- **Keys are sorted and unique per frame.** Adding at an existing offset
  replaces that key. Height snaps to the current rung list at authoring only;
  the model accepts any valid height, so loosening later needs no schema
  change.
- **`w/h` step, `x/y` animate.** A pass has constant pixel geometry; a key
  whose size differs from the previous one starts a new pass. Between keys
  x/y interpolate with a filter expression. Before the first key the static
  size is held and x/y move toward the first key, clamped to the available
  travel (the window never pre-shrinks); after the last key it holds.
- **One planner.** `timeline::crop_passes` owns the ranges and windows: the
  exporter only formats its passes, the CV checks sample
  `CropPass::window_at`, `crop_stable` takes its movement/rate from the passes
  (not raw key spacing), and `crop_window_at` (rebase) samples it too.
  Validation rejects negative/non-increasing keys at load/save **and** the
  effect edit boundary (`InvalidEffect`); the planner asserts that order
  before sorting and skips a negative start defensively in release.
- **The head is the origin.** Split/trim-in/head-extend re-base keys onto the
  new head (the cut value becomes the key at 0); retime scales key offsets so
  the same source frame keeps its key. A tail trim keeps a key after the new
  end as the final pass's endpoint (interpolation runs to the clip end); a
  lone past-end key leaves the base held.
- **Tail cuts synthesize the endpoint.** Split/slice/left-tail uses the
  planner at the cut (against the **pre-cut duration**), clamps the sampled
  window into the frame (`x+w ≤ 100`, `y+h ≤ 100`) and appends it as a key at
  the cut when none sits exactly there, then drops later keys. Both halves
  therefore meet on the same cut window. Every window `rebase_crop_keys`
  writes back is clamped the same way, because a pass keeps the earlier key's
  size while x/y move toward a differently sized next key.
- **Deferred: cap-crossing size steps.** A pass with a shrinking
  next key can cross the `100−w`/`100−h` cap *before* a cut. Cuts clamp the
  sampled endpoint so the written key is valid, but no key is synthesized at
  the crossing, so the left ramp before that cut can still differ from the
  un-cut render. Pixel-level export parity is deferred alongside it.
- **A keyed crop is alone.** Adding another effect to the clip is refused
  (`KeyedCropExclusive`), and so is a transition beside it
  (`KeyedCropTransition`); the renderer repeats the refusal as a backstop.
  A static `effect crop` edit over keys is refused
  (`CropKeysWouldBeLost`) — add a key, or clear the effect first.
- **Height snapping respects `y`.** A key chooses the nearest rung that fits
  below `y`; no fitting rung is `CropKeyHeight`, not a corrupt timeline.
- **Passes are frame-aligned and concatenated**, cropped/padded back to the
  canvas with `overlay`, then normalized to the output size. Proxy hashes
  serialize the clip, so the key list is part of the cache key.
