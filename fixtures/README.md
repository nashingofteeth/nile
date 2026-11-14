# 2-cam fixture show

A tiny synthetic show for the golden harness. Media is generated at test time
(never committed): `cam_a.mp4` (red, 6 s), `cam_b.mp4` (green, 6 s), and
`mic.m4a` (sine, 6 s), all 320×240@30.

| File | Job |
|---|---|
| `show.edl` | CMX3600 cut list: A 0–2, B 0–2, A 2–4 (record 0–6) |
| `show.otio` | The same show as a Resolve/Kdenlive-style OTIO timeline, plus the mic audio track |
| `atem.log` | Synthetic switcher log: program bus cuts at 0, 2, 4 s |
| `expected/project.json` | Normalized project shape (asset names, source spans, track order) |
| `expected/monitor.edl` | Normalized video-track EDL: file, source offset, timeline length |
| `expected/frames.txt` | Expected export frame count (6 s @ 30 fps) |

The harness is `crates/nle-cli/tests/fixtures.rs`; `cargo test` (and therefore
`scripts/check.sh`) runs it. It imports the EDL and OTIO, checks both against
the golden project, checks the ATEM log against the cuts, exports, compares
the frame count, and samples PSNR against a program reference rendered from
the same sources.

Update the golden files deliberately: a changed cut, source span, track order,
or export geometry is a fixture change, not a test tweak.
