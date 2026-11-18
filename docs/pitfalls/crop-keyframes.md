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
  window is the geometry, interpolating toward the first key; after the last
  key it holds.
- **A keyed crop is alone.** The renderer refuses another effect on the clip
  (`KeyedCropEffects`) and refuses a transition side
  (`KeyedCropTransition`): the pass graph has no place for a shared chain.
- **Passes are frame-aligned and concatenated**, cropped/padded back to the
  canvas with `overlay`, then normalized to the output size. Proxy hashes
  serialize the clip, so the key list is part of the cache key.
