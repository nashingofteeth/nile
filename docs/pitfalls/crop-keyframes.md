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
- **The head is the origin.** Split/trim-in/head-extend re-base keys onto the
  new head (the cut value becomes the key at 0); retime scales key offsets so
  the same source frame keeps its key. Tail trims leave keys after the new
  end inert.
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
