//! The timeline: tracks of sequential segments — clips and gaps.
//!
//! A track is a *sequence*: segments play back-to-back, and a clip's
//! start is DERIVED (the sum of the durations before it). Overlaps are
//! unrepresentable; gaps — leading, interior, trailing — are ordinary
//! segments, not special state. Removing a segment IS a ripple; replacing
//! a clip with a gap IS "leave a gap".

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{
    Seconds, checked_add, checked_div, checked_sub, format_seconds, rational_ser, secs_f64,
};

#[derive(Debug, Error)]
pub enum TimelineError {
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no segment at index {0}")]
    SegmentNotFound(usize),
    #[error("timeline corrupt: {0}")]
    Corrupt(String),
    #[error("a track is {len} s long — the timeline limit is {limit} s")]
    TooLong { len: String, limit: i64 },
}

/// The longest timeline we accept, in seconds — a day. Not a workflow
/// limit (nobody edits a day): it is the guard that keeps duration sums
/// (i64 rationals) from overflowing, which wraps a track's length
/// negative in release builds and panics in debug ones.
pub const MAX_TIMELINE_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One crop-window keyframe. `at` is clip-relative timeline seconds (after
/// speed); x/y/w/h use the same percentages as the static window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CropKey {
    #[serde(with = "rational_ser")]
    pub at: Seconds,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Closed height rungs for authoring keyframes. This is deliberately only a
/// front-end restriction: removing the list later accepts any valid height
/// without a schema change.
pub const CROP_RUNGS: &[f64] = &[20.0, 25.0, 33.0, 40.0, 50.0, 60.0, 66.0, 75.0, 80.0, 100.0];

impl CropKey {
    /// The nearest allowed height that still fits below `y`, for an
    /// authoring path. `None` when no rung fits.
    pub fn snapped_h(&self) -> Option<f64> {
        CROP_RUNGS
            .iter()
            .copied()
            .filter(|rung| *rung <= 100.0 - self.y)
            .min_by(|a, b| (a - self.h).abs().total_cmp(&(b - self.h).abs()))
    }
}

/// A per-clip video effect, applied in export after the retime and before
/// the geometry normalize. Typed (not raw ffmpeg fragments) so it stays
/// serializable and safe to hand to the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// A crop window, as percentages of the picture at this point in the
    /// chain: `x`/`y` are the top-left corner, `w`/`h` the window size. The kept picture stays where
    /// it is; the surrounding strips become transparent, so an upper track
    /// shows through them (black when nothing is below).
    Crop {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        /// Keyframes at clip-relative timeline seconds. Empty means a static
        /// window using the fields above.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<CropKey>,
    },
    /// `scale`, as percentages of the picture at this point in the chain
    /// (100 = unchanged). Below 100% it shrinks about its centre and pads
    /// transparently, so a lower track shows through; above 100% is a zoom.
    Scale { w: f64, h: f64 },
    /// `offset`, as percentages of the distance from the picture's current
    /// position to the edge: `x` moves right (negative left), `y` moves down
    /// (negative up). 0 is no move and ±100 puts it fully off-screen; the
    /// vacated area is transparent, so a lower track shows through.
    Offset { x: f64, y: f64 },
    /// `eq=brightness=…:contrast=…:saturation=…`.
    Eq {
        brightness: f64,
        contrast: f64,
        saturation: f64,
    },
    /// `lut3d=file=…`.
    Lut { path: String },
}

impl Effect {
    /// The short name used by notices, `nile show`, and the effects panel.
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Crop { .. } => "crop",
            Effect::Scale { .. } => "scale",
            Effect::Offset { .. } => "offset",
            Effect::Eq { .. } => "eq",
            Effect::Lut { .. } => "lut",
        }
    }

    /// Does this effect carry crop keyframes?
    pub fn is_keyed_crop(&self) -> bool {
        matches!(
            self,
            Effect::Crop { keys, .. } if !keys.is_empty()
        )
    }

    /// Why this effect cannot be persisted, or `None` when it is valid.
    pub fn validation_error(&self) -> Option<&'static str> {
        if !self.is_finite() {
            return Some("effect parameter is non-finite");
        }
        match self {
            Effect::Crop { x, y, w, h, keys } => {
                let bad = |x: f64, y: f64, w: f64, h: f64| {
                    x < 0.0 || y < 0.0 || w <= 0.0 || h <= 0.0 || x + w > 100.0 || y + h > 100.0
                };
                if bad(*x, *y, *w, *h)
                    || keys.iter().any(|key| bad(key.x, key.y, key.w, key.h))
                    || keys.iter().any(|key| key.at < Seconds::from_integer(0))
                    || keys.windows(2).any(|pair| pair[0].at >= pair[1].at)
                {
                    Some("effect parameter is out of range")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Every parameter is finite and inside its range.
    pub fn is_valid(&self) -> bool {
        self.validation_error().is_none()
    }

    /// Every numeric parameter is finite.
    pub fn is_finite(&self) -> bool {
        match self {
            Effect::Crop { x, y, w, h, keys } => {
                [x, y, w, h].iter().all(|v| v.is_finite())
                    && keys
                        .iter()
                        .all(|key| [key.x, key.y, key.w, key.h].iter().all(|v| v.is_finite()))
            }
            Effect::Scale { w, h } => w.is_finite() && h.is_finite(),
            Effect::Offset { x, y } => x.is_finite() && y.is_finite(),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => brightness.is_finite() && contrast.is_finite() && saturation.is_finite(),
            Effect::Lut { .. } => true,
        }
    }

    /// The parameters as one line — the panel's detail column.
    pub fn details(&self) -> String {
        match self {
            Effect::Crop { x, y, w, h, keys } if !keys.is_empty() => {
                format!("{w}×{h} @ {x},{y} · {} key(s)", keys.len())
            }
            Effect::Crop { x, y, w, h, .. } => format!("{w}×{h} @ {x},{y}"),
            Effect::Scale { w, h } => format!("{w}% × {h}%"),
            Effect::Offset { x, y } => format!("x {x}% · y {y}%"),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => format!("b {brightness} · c {contrast} · s {saturation}"),
            // The full path is noise in a one-line row; the file name is the
            // part that identifies the LUT.
            Effect::Lut { path } => std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
        }
    }

    /// The ffmpeg filters for a whole chain, comma-joined (empty when nothing
    /// renders). The picture box is threaded through: `scale` resizes the
    /// current box about its centre, `crop` selects a window inside it, and colour
    /// effects leave it alone. Anything outside the box is transparent, so a
    /// lower track shows through (black when none does — the classic
    /// single-track graph drops alpha at the end).
    pub fn chain(effects: &[Effect], canvas: (u32, u32)) -> String {
        let mut picture = Picture::full(canvas);
        let mut parts: Vec<String> = Vec::new();
        for effect in effects {
            let (filter, next) = effect.apply(canvas, picture);
            if !filter.is_empty() {
                parts.push(filter);
            }
            picture = next;
        }
        parts.join(",")
    }

    /// The picture geometry a whole chain leaves on `canvas`: the visible box
    /// and the projection from the source frame's normalized `[0, 1]`
    /// coordinates. This is the same tracker [`Effect::chain`] builds its
    /// filters from, so a check can never drift from the rendered picture.
    ///
    /// A keyed crop renders through its own pass graph: the exporter's
    /// `effect_chain` refuses it, and this models the static chain only (the
    /// crop's static fields). Callers that see keyframes must use the keyed
    /// window at the sampled time, not this result.
    pub fn geometry(effects: &[Effect], canvas: (u32, u32)) -> EffectGeometry {
        let mut picture = Picture::full(canvas);
        for effect in effects {
            picture = effect.apply(canvas, picture).1;
        }
        EffectGeometry { picture }
    }

    /// Does the chain leave any transparent canvas? The monitor composites the
    /// layer below when it does; the export's overlay handles it directly.
    pub fn masks(effects: &[Effect], canvas: (u32, u32)) -> bool {
        if effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::Crop { keys, .. }
                    if keys.iter().any(|key| {
                        key.x != 0.0 || key.y != 0.0 || key.w != 100.0 || key.h != 100.0
                    })
            )
        }) {
            return true;
        }
        let mut picture = Picture::full(canvas);
        for effect in effects {
            picture = effect.apply(canvas, picture).1;
        }
        !picture.content.is_full(canvas)
    }

    /// One effect: its filter and the picture box it leaves behind. Computing
    /// both together keeps the emitted geometry and the tracked box from
    /// drifting apart.
    fn apply(&self, canvas: (u32, u32), picture: Picture) -> (String, Picture) {
        match self {
            Effect::Crop { x, y, w, h, .. } => {
                if picture.content.is_empty() {
                    return (String::new(), picture); // already invisible
                }
                let next = crop_content(*x, *y, *w, *h, picture.content);
                if next == picture.content {
                    return (String::new(), picture); // no inset: nothing to mask
                }
                let (fw, fh) = canvas;
                let (x, y) = (next.x, next.y);
                (
                    format!(
                        "format=yuva420p,crop={}:{}:{x}:{y},\
                         pad={fw}:{fh}:{x}:{y}:color=black@0",
                        next.w, next.h,
                    ),
                    Picture {
                        content: next,
                        changed: true,
                        ..picture
                    },
                )
            }
            Effect::Scale { w, h } => {
                if picture.content.is_empty() {
                    (String::new(), picture) // already invisible
                } else {
                    scale_content(*w, *h, canvas, picture)
                }
            }
            Effect::Offset { x, y } => offset_content(*x, *y, canvas, picture),
            Effect::Eq {
                brightness,
                contrast,
                saturation,
            } => (
                format!("eq=brightness={brightness}:contrast={contrast}:saturation={saturation}"),
                picture,
            ),
            // The path crosses two parsers, so it takes two escapes in this
            // order: the option parser reads `\` as an escape (so `\`, `:` and
            // `'` each need one), then the graph parser needs the whole value
            // quoted, where a literal `'` is `'\''`. Escaping the graph level
            // first would let it eat the option-level backslashes.
            Effect::Lut { path } => {
                let mut option = String::with_capacity(path.len() + 4);
                for ch in path.chars() {
                    if matches!(ch, '\\' | ':' | '\'') {
                        option.push('\\');
                    }
                    option.push(ch);
                }
                let quoted = option.replace('\'', r"'\''");
                (format!("lut3d=file='{quoted}'"), picture)
            }
        }
    }
}

/// The visible picture and its source projection after an effect chain; see
/// [`Effect::geometry`]. Rectangle positions are canvas pixels, and projection
/// inputs are fractions of the source frame (`0.0..=1.0`). The source frame is
/// the same size as the canvas: effects run before the output normalize scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectGeometry {
    picture: Picture,
}

impl EffectGeometry {
    /// The visible picture box on the canvas: `(x, y, w, h)` in pixels.
    pub fn visible(&self) -> (i64, i64, i64, i64) {
        let content = self.picture.content;
        (content.x, content.y, content.w, content.h)
    }

    /// No picture pixels reach the canvas; the layer is fully transparent.
    pub fn is_empty(&self) -> bool {
        self.picture.content.is_empty()
    }

    /// Did a geometry effect actually rewrite the picture? A no-op scale or a
    /// full-window crop leaves the frame untouched, exactly like no effect.
    pub fn changes_picture(&self) -> bool {
        self.picture.changed
    }

    /// Map a source-frame rectangle (fractions, `0.0..=1.0`) to canvas pixels:
    /// `(x, y, w, h)`.
    pub fn project(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let map = self.picture.map;
        let x0 = map.scale_x * x + map.offset_x;
        let y0 = map.scale_y * y + map.offset_y;
        let x1 = map.scale_x * (x + w) + map.offset_x;
        let y1 = map.scale_y * (y + h) + map.offset_y;
        (x0, y0, x1 - x0, y1 - y0)
    }
}

/// The chain's source-frame → canvas projection, per axis:
/// `canvas = scale × source + offset`. `source` is a normalized `[0, 1]`
/// coordinate, so the identity over a `W`-wide canvas has `scale = W`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Map {
    scale_x: f64,
    offset_x: f64,
    scale_y: f64,
    offset_y: f64,
}

impl Map {
    fn full((w, h): (u32, u32)) -> Self {
        Self {
            scale_x: f64::from(w),
            offset_x: 0.0,
            scale_y: f64::from(h),
            offset_y: 0.0,
        }
    }
}

/// The chain's working state: the visible box and the projection that put it
/// there. `changed` separates a no-op chain (full frame, identity projection)
/// from a zoom that was clipped back to the full canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Picture {
    content: Content,
    map: Map,
    changed: bool,
}

impl Picture {
    fn full(canvas: (u32, u32)) -> Self {
        Self {
            content: Content::full(canvas),
            map: Map::full(canvas),
            changed: false,
        }
    }
}

/// The visible picture inside the effect canvas, in canvas pixels. `x`/`y`
/// are non-negative and the box is clipped to the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Content {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Content {
    fn full((w, h): (u32, u32)) -> Self {
        Self {
            x: 0,
            y: 0,
            w: i64::from(w),
            h: i64::from(h),
        }
    }

    /// No picture left on the canvas: the frame is fully transparent.
    fn empty() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    fn is_full(&self, canvas: (u32, u32)) -> bool {
        *self == Self::full(canvas)
    }
}

/// A frame that is entirely transparent black. Used when a geometry effect
/// leaves nothing on the canvas (e.g. offset 100%). Zeroing RGB as well as
/// alpha matters: the single-track export drops alpha to black.
fn transparent_filter() -> String {
    "format=yuva420p,colorchannelmixer=rr=0:gg=0:bb=0:aa=0".to_string()
}

/// A window inside `content`: `x`/`y` are percentages from its top-left and
/// `w`/`h` percentages of its size. Clamped so a hand-edited project cannot
/// crop past the picture.
fn crop_content(x: f64, y: f64, w: f64, h: f64, content: Content) -> Content {
    if content.is_empty() {
        return content;
    }
    let pct = |size: i64, value: f64| {
        ((size as f64 * value / 100.0).round() as i64).clamp(0, size.max(0))
    };
    let left = pct(content.w, x).min(content.w);
    let top = pct(content.h, y).min(content.h);
    let width = pct(content.w, w).clamp(2, content.w.max(2));
    let height = pct(content.h, h).clamp(2, content.h.max(2));
    let w = width.min((content.w - left).max(2));
    let h = height.min((content.h - top).max(2));
    let l = left;
    let t = top;
    Content {
        x: content.x + l.min((content.w - w).max(0)),
        y: content.y + t.min((content.h - h).max(0)),
        w,
        h,
    }
}

/// Resize the current box to `w`/`h` percent of itself, about its centre. The
/// scaled picture is clipped to the canvas and padded transparently around it,
/// so a lower track shows through; above 100% it is a centre zoom.
fn scale_content(wpct: f64, hpct: f64, canvas: (u32, u32), picture: Picture) -> (String, Picture) {
    let content = picture.content;
    if content.is_empty() {
        return (String::new(), picture);
    }
    let (fw, fh) = (i64::from(canvas.0), i64::from(canvas.1));
    let size = |base: i64, pct: f64| {
        ((base as f64 * pct / 100.0).round() as i64).clamp(2, i64::from(u32::MAX / 4))
    };
    let (sw, sh) = (size(content.w, wpct), size(content.h, hpct));
    // Keep the box's centre (doubled, so an odd size stays integral).
    let sx = (2 * content.x + content.w - sw) / 2;
    let sy = (2 * content.y + content.h - sh) / 2;
    // Clip the scaled picture to the canvas.
    let ix = sx.max(0);
    let iy = sy.max(0);
    let w = (sx + sw).min(fw) - ix;
    let h = (sy + sh).min(fh) - iy;
    if w < 2 || h < 2 {
        return (
            transparent_filter(),
            Picture {
                content: Content::empty(),
                changed: true,
                ..picture
            },
        );
    }
    let next = Content {
        x: ix.min(fw - w),
        y: iy.min(fh - h),
        w,
        h,
    };
    // 100% (or a rounding no-op) renders nothing.
    if sw == content.w && sh == content.h && next == content {
        return (String::new(), picture);
    }
    // Extend the projection by the same resize: the scaled box starts at
    // `sx`, and a source point already on the canvas moves with it.
    let kx = sw as f64 / content.w as f64;
    let ky = sh as f64 / content.h as f64;
    let map = Map {
        scale_x: picture.map.scale_x * kx,
        offset_x: sx as f64 + (picture.map.offset_x - content.x as f64) * kx,
        scale_y: picture.map.scale_y * ky,
        offset_y: sy as f64 + (picture.map.offset_y - content.y as f64) * ky,
    };
    let mut chain = String::from("format=yuva420p");
    if !content.is_full(canvas) {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            content.w, content.h, content.x, content.y
        );
    }
    chain += &format!(",scale={sw}:{sh}");
    // The scaled picture can stick out of the canvas: crop the overflow.
    if next.x != sx || next.y != sy || next.w != sw || next.h != sh {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            next.w,
            next.h,
            next.x - sx,
            next.y - sy
        );
    }
    if !next.is_full(canvas) {
        chain += &format!(",pad={fw}:{fh}:{}:{}:color=black@0", next.x, next.y);
    }
    (
        chain,
        Picture {
            content: next,
            map,
            changed: true,
        },
    )
}

/// Move the current box toward an edge by `x`/`y` percent of the distance to
/// it. 0 is no move, ±100 puts the picture fully off-canvas (a transparent
/// frame); intermediate values leave the rest of the picture on screen.
fn offset_content(xpct: f64, ypct: f64, canvas: (u32, u32), picture: Picture) -> (String, Picture) {
    let content = picture.content;
    if content.is_empty() {
        return (String::new(), picture);
    }
    let (fw, fh) = (i64::from(canvas.0), i64::from(canvas.1));
    // Right/up use the room to that edge; left/down use the room to the
    // other one, so ±100 always reaches the edge from where the picture is.
    let dx = if xpct >= 0.0 {
        ((fw - content.x) as f64 * xpct / 100.0).round() as i64
    } else {
        -(((content.x + content.w) as f64) * -xpct / 100.0).round() as i64
    };
    let dy = if ypct >= 0.0 {
        ((fh - content.y) as f64 * ypct / 100.0).round() as i64
    } else {
        -(((content.y + content.h) as f64) * -ypct / 100.0).round() as i64
    };
    if dx == 0 && dy == 0 {
        return (String::new(), picture);
    }
    // The casts saturate (a hand-edited `1e300` percent becomes
    // `i64::MAX`), so the i64 adds must not assume the add stays in range:
    // `x + i64::MAX` overflow-panicked the debug build and wrapped into
    // garbage crop/pad coordinates in release. Saturate instead — the
    // result is "the picture moved to or past the edge", which is what a
    // huge percentage means.
    let nx = content.x.saturating_add(dx);
    let ny = content.y.saturating_add(dy);
    // The part of the moved picture still on the canvas.
    let ix = nx.max(0);
    let iy = ny.max(0);
    let vw = nx.saturating_add(content.w).min(fw).saturating_sub(ix);
    let vh = ny.saturating_add(content.h).min(fh).saturating_sub(iy);
    if vw < 2 || vh < 2 {
        // Fully (or within a pixel) off canvas: a transparent layer. At
        // ±100% of the canvas the picture is entirely outside.
        return (
            transparent_filter(),
            Picture {
                content: Content::empty(),
                changed: true,
                ..picture
            },
        );
    }
    // Source offset inside the picture and destination on the canvas.
    let sx = ix.saturating_sub(nx);
    let sy = iy.saturating_sub(ny);
    let next = Content {
        x: ix,
        y: iy,
        w: vw,
        h: vh,
    };
    let map = Map {
        offset_x: picture.map.offset_x + dx as f64,
        offset_y: picture.map.offset_y + dy as f64,
        ..picture.map
    };
    let mut chain = String::from("format=yuva420p");
    if !content.is_full(canvas) {
        chain += &format!(
            ",crop={}:{}:{}:{}",
            content.w, content.h, content.x, content.y
        );
    }
    if vw != content.w || vh != content.h {
        chain += &format!(",crop={vw}:{vh}:{sx}:{sy}");
    }
    chain += &format!(",pad={fw}:{fh}:{}:{}:color=black@0", next.x, next.y);
    (
        chain,
        Picture {
            content: next,
            map,
            changed: true,
        },
    )
}

/// One piece of media on a track. Its timeline position is derived from
/// the segments before it — a clip only owns its source range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u32,
    pub asset: u32,
    #[serde(with = "rational_ser")]
    pub asset_in: Seconds,
    #[serde(with = "rational_ser")]
    pub asset_out: Seconds,
    /// Playback rate relative to the source: timeline duration is
    /// `(asset_out - asset_in) / speed`. `1` is normal speed; a Kdenlive
    /// rate conform (e.g. 24 fps project over 29.97 fps media) is
    /// `project_rate / media_rate`. Never zero or negative.
    #[serde(with = "rational_ser")]
    pub speed: Seconds,
    /// Per-clip video effects, applied in order at export.
    #[serde(default)]
    pub effects: Vec<Effect>,
}

/// A resolved crop window, in percentages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropWindow {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Effect {
    /// The crop window at clip-relative timeline `at`, sampled from the same
    /// [`crop_passes`] plan the exporter renders. `duration` matters at the
    /// tail: a key past the clip end is the final pass's endpoint, so the
    /// interpolation runs to the clip end, not to that key's own time.
    ///
    /// Before the head ramp the static window is returned (a head extension
    /// has no rendered frame there); after the last pass the final window is
    /// held by [`CropPass::window_at`]. A key exactly at `at` returns its own
    /// window even at the clip end, where no rendered pass starts: a
    /// split/trim there re-bases onto that key's size.
    pub fn crop_window_at(&self, at: Seconds, duration: Seconds) -> Option<CropWindow> {
        let Effect::Crop { x, y, w, h, keys } = self else {
            return None;
        };
        // A key exactly at the sampled time is the state at that instant. It
        // matters at the clip end: the renderer starts no pass for a key at
        // `duration`, but a split/trim landing there re-bases the right-hand
        // clip onto that key's own size.
        if let Some(key) = keys.iter().find(|key| key.at == at) {
            return Some(CropWindow {
                x: key.x,
                y: key.y,
                w: key.w,
                h: key.h,
            });
        }
        if at <= Seconds::from_integer(0) {
            return Some(CropWindow {
                x: *x,
                y: *y,
                w: *w,
                h: *h,
            });
        }
        crop_passes(self, duration)
            .iter()
            .rev()
            .find(|pass| pass.start <= at)
            .map(|pass| pass.window_at(at))
    }
}

/// One planned keyed-crop pass, in clip-relative timeline seconds. `x`/`y`
/// interpolate from `(x0, y0)` to `(x1, y1)` over `[start, end)` while `w`/`h`
/// stay constant: the renderer steps size at a key, it does not interpolate
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropPass {
    pub start: Seconds,
    pub end: Seconds,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub w: f64,
    pub h: f64,
}

impl CropPass {
    /// The window at `at`, clamped to the pass. A caller sampling a point
    /// only needs the pass containing it, but clamping keeps an out-of-range
    /// sample honest instead of extrapolating.
    pub fn window_at(&self, at: Seconds) -> CropWindow {
        let span = secs_f64(&(self.end - self.start));
        let fraction = if span > 0.0 {
            (secs_f64(&(at - self.start)) / span).clamp(0.0, 1.0)
        } else {
            1.0
        };
        CropWindow {
            x: self.x0 + (self.x1 - self.x0) * fraction,
            y: self.y0 + (self.y1 - self.y0) * fraction,
            w: self.w,
            h: self.h,
        }
    }
}

/// The keyed-crop pass plan the exporter renders and the CV checks sample.
/// Returns one pass for a static crop, otherwise a base→first-key head ramp,
/// one pass per key pair (constant size, interpolated x/y) and the last
/// window held to `duration`.
///
/// Keys are expected valid: `Effect::validation_error` rejects negatives and
/// non-increasing offsets before a project loads or saves, so this planner
/// debug-asserts that assumption (before sorting) instead of silently
/// dropping the head ramp. A key at/past `duration` never starts a pass: a
/// lone one leaves the base held for the whole clip, while one after an
/// in-range key is that final pass's endpoint, so the interpolation runs to
/// the clip end.
pub fn crop_passes(effect: &Effect, duration: Seconds) -> Vec<CropPass> {
    let Effect::Crop { x, y, w, h, keys } = effect else {
        return Vec::new();
    };
    let base = CropWindow {
        x: *x,
        y: *y,
        w: *w,
        h: *h,
    };
    let keys: Vec<&CropKey> = keys.iter().collect();
    let zero = Seconds::from_integer(0);
    debug_assert!(
        keys.iter().all(|key| key.at >= zero),
        "crop key before the clip head: validation must reject it"
    );
    debug_assert!(
        keys.windows(2).all(|pair| pair[0].at < pair[1].at),
        "crop keys must be strictly increasing: validation must reject it"
    );
    // Valid callers already hand us sorted keys; sorting here keeps a release
    // build with a corrupt in-memory effect from emitting passes out of order.
    let mut keys = keys;
    keys.sort_by_key(|key| key.at);
    keyed_passes(base, &keys, duration)
}

/// The actual plan over an ordered key slice. Kept separate from
/// [`crop_passes`] so the release-safe negative-start skip can be tested
/// without tripping the debug assertion above.
fn keyed_passes(base: CropWindow, keys: &[&CropKey], duration: Seconds) -> Vec<CropPass> {
    let zero = Seconds::from_integer(0);
    let Some(first) = keys.first() else {
        return vec![CropPass {
            start: zero,
            end: duration,
            x0: base.x,
            y0: base.y,
            x1: base.x,
            y1: base.y,
            w: base.w,
            h: base.h,
        }];
    };
    let mut passes = Vec::new();
    if first.at > zero {
        let end = first.at.min(duration);
        if end > zero {
            // A first key past the clip's end never arrives: the base holds.
            let (x1, y1) = if first.at <= duration {
                (first.x, first.y)
            } else {
                (base.x, base.y)
            };
            passes.push(CropPass {
                start: zero,
                end,
                x0: base.x,
                y0: base.y,
                x1,
                y1,
                w: base.w,
                h: base.h,
            });
        }
    }
    for (index, key) in keys.iter().enumerate() {
        // Defence in depth for a corrupt in-memory effect (validation runs at
        // load/save and at the edit boundary): never emit a negative trim.
        if key.at < zero || key.at >= duration {
            continue;
        }
        let end = keys
            .get(index + 1)
            .map(|next| next.at.min(duration))
            .unwrap_or(duration);
        if end <= key.at {
            continue;
        }
        let (x1, y1) = keys
            .get(index + 1)
            .map(|next| (next.x, next.y))
            .unwrap_or((key.x, key.y));
        passes.push(CropPass {
            start: key.at,
            end,
            x0: key.x,
            y0: key.y,
            x1,
            y1,
            w: key.w,
            h: key.h,
        });
    }
    passes
}

/// Pull a sampled window inside the frame on both axes. A pass keeps the
/// starting key's size while x/y interpolate toward a differently sized next
/// key, so the sampled rectangle can overhang the canvas; writing that window
/// back as a key would make `Effect::validation_error` reject the edit.
fn clamp_crop_window(window: CropWindow) -> CropWindow {
    CropWindow {
        x: window.x.clamp(0.0, (100.0 - window.w).max(0.0)),
        y: window.y.clamp(0.0, (100.0 - window.h).max(0.0)),
        w: window.w,
        h: window.h,
    }
}

impl Clip {
    /// Re-base crop keys after `removed` timeline seconds disappear from the
    /// head (negative grows it). The value at the new head becomes the key at
    /// 0, so a split or trim does not restart the animation from the old base.
    /// A static crop (no keys) stays static: turning its base window into a
    /// key would make it keyed, and a keyed crop cannot share a clip with
    /// another effect.
    ///
    /// `duration` is the *pre-cut* clip duration: callers have already
    /// adjusted `asset_in`, so `self.duration()` alone is too short and would
    /// compress a pass that ended at the old end.
    pub fn rebase_crop_keys(&mut self, removed: Seconds, duration: Seconds) {
        if removed == Seconds::from_integer(0) {
            return;
        }
        for effect in &mut self.effects {
            if matches!(effect, Effect::Crop { keys, .. } if keys.is_empty()) {
                continue;
            }
            let Some(value) = effect.crop_window_at(removed, duration) else {
                continue;
            };
            let value = clamp_crop_window(value);
            let Effect::Crop { keys, .. } = effect else {
                continue;
            };
            keys.retain(|key| key.at > removed);
            for key in keys.iter_mut() {
                key.at -= removed;
            }
            keys.push(CropKey {
                at: Seconds::from_integer(0),
                x: value.x,
                y: value.y,
                w: value.w,
                h: value.h,
            });
            keys.sort_by_key(|key| key.at);
        }
    }

    /// End the crop at `at` for the left half of a split/slice. `duration` is
    /// the *pre-cut* length. The cut's sampled window is appended as a key
    /// when no key sits exactly there, so the left half's last pass ends on
    /// the same window the right half re-bases to; keys after the cut are then
    /// dropped. Without the endpoint the final pass would hold the previous
    /// key and pop at the seam.
    pub fn truncate_crop_keys(&mut self, at: Seconds, duration: Seconds) {
        for effect in &mut self.effects {
            let Effect::Crop { keys, .. } = effect else {
                continue;
            };
            if keys.is_empty() {
                continue;
            }
            let endpoint = if keys.iter().any(|key| key.at == at) {
                None
            } else {
                effect.crop_window_at(at, duration).map(clamp_crop_window)
            };
            let Effect::Crop { keys, .. } = effect else {
                continue;
            };
            if let Some(value) = endpoint {
                keys.push(CropKey {
                    at,
                    x: value.x,
                    y: value.y,
                    w: value.w,
                    h: value.h,
                });
            }
            keys.retain(|key| key.at <= at);
            keys.sort_by_key(|key| key.at);
        }
    }

    /// Scale key offsets after a retime. `ratio` is old duration / new
    /// duration, so material that was at timeline `t` stays at the same
    /// source frame.
    pub fn scale_crop_keys(&mut self, ratio: Seconds) {
        if ratio <= Seconds::from_integer(0) {
            return;
        }
        for effect in &mut self.effects {
            if let Effect::Crop { keys, .. } = effect {
                for key in keys.iter_mut() {
                    key.at *= ratio;
                }
            }
        }
    }

    /// A normal-speed clip (`speed = 1`) with no effects.
    pub fn new(id: u32, asset: u32, asset_in: Seconds, asset_out: Seconds) -> Self {
        Self {
            id,
            asset,
            asset_in,
            asset_out,
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }
    }

    /// The source span the clip reads, before the speed factor.
    pub fn source_duration(&self) -> Seconds {
        self.asset_out - self.asset_in
    }

    /// The timeline span the clip occupies (source scaled by `speed`).
    pub fn duration(&self) -> Seconds {
        self.source_duration() / self.speed
    }

    /// [`Clip::duration`] with checked arithmetic: `None` when the source
    /// span or the division by `speed` exceeds the exact rational. A clip can
    /// be *validated* (its reduced duration fits) yet still make the raw
    /// subtraction overflow on a large common denominator — an extreme rate
    /// leaves asset bounds like `1 - 1/M`.
    pub(crate) fn checked_duration(&self) -> Option<Seconds> {
        checked_div(&checked_sub(&self.asset_out, &self.asset_in)?, &self.speed)
    }

    /// Source time at a timeline offset inside the clip (timeline →
    /// source). Edits move in timeline time but `asset_in/out` live in
    /// source time, so crossing that boundary scales by `speed`.
    pub fn source_at(&self, timeline_offset: Seconds) -> Seconds {
        source_time(self.asset_in, self.speed, timeline_offset)
    }

    /// Timeline offset of a source time inside the clip (source →
    /// timeline) — the inverse of [`Clip::source_at`].
    pub fn timeline_offset_of(&self, source: Seconds) -> Seconds {
        offset_of_source(self.asset_in, self.speed, source)
    }
}

/// Source time at a timeline offset, given a span's `asset_in` and `speed` —
/// the one definition of the timeline↔source boundary. A `Clip` calls this;
/// so does a recorded `FrameSource`, which has no `Clip` to ask.
pub fn source_time(asset_in: Seconds, speed: Seconds, timeline_offset: Seconds) -> Seconds {
    asset_in + timeline_offset * speed
}

/// The inverse of [`source_time`]: a source time's offset from the span start.
pub fn offset_of_source(asset_in: Seconds, speed: Seconds, source: Seconds) -> Seconds {
    (source - asset_in) / speed
}

/// Absolute timeline position of `clip`'s `source` time, across tracks.
/// One helper so the annotation resolver and the model agree.
pub fn clip_position(tracks: &[Track], clip: u32, source: Seconds) -> Option<Seconds> {
    for track in tracks {
        for (index, segment) in track.segments.iter().enumerate() {
            if let Segment::Clip(c) = segment
                && c.id == clip
            {
                return Some(track.start_of(index) + c.timeline_offset_of(source));
            }
        }
    }
    None
}

/// A stretch of nothing: black + silence. Leading, interior, and
/// trailing space are all just Gap segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    #[serde(with = "rational_ser")]
    pub duration: Seconds,
}

/// Where one side of a transition comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransitionSource {
    /// A trimmed span of a pooled asset, in source seconds. Rendered into
    /// `dur` of timeline, so the side's rate is implied by the span.
    Clip {
        asset: u32,
        #[serde(with = "rational_ser")]
        src_in: Seconds,
        #[serde(with = "rational_ser")]
        src_out: Seconds,
        /// The neighbour's effects, copied when the side was built and kept in
        /// sync when the neighbour's list changes: the transition renders that
        /// neighbour's material, effects included.
        #[serde(default)]
        effects: Vec<Effect>,
    },
    /// Generated black — toward a gap or the timeline edge.
    Black,
}

impl TransitionSource {
    /// The pooled asset this side reads, if it is a clip side.
    pub fn asset(&self) -> Option<u32> {
        match self {
            TransitionSource::Clip { asset, .. } => Some(*asset),
            TransitionSource::Black => None,
        }
    }
}

/// The crossfade shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    /// `xfade=transition=fade` — blend the outgoing side into the incoming.
    Dissolve,
    /// `xfade=transition=fadeblack` — fade through black.
    FadeBlack,
}

/// A transition between two source sides, occupying `dur` of timeline. It
/// *owns* both source spans (as a clip owns its own), so it never dangles
/// when neighbours change. A transition needs at least one clip side —
/// black↔black is meaningless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub id: u32,
    #[serde(with = "rational_ser")]
    pub dur: Seconds,
    pub kind: TransitionKind,
    /// Outgoing (left) side.
    pub a: TransitionSource,
    /// Incoming (right) side.
    pub b: TransitionSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    Clip(Clip),
    Gap(Gap),
    Transition(Transition),
}

impl Segment {
    pub fn duration(&self) -> Seconds {
        match self {
            Segment::Clip(c) => c.duration(),
            Segment::Gap(g) => g.duration,
            Segment::Transition(t) => t.dur,
        }
    }

    /// [`Segment::duration`], or `None` if the arithmetic would overflow.
    /// `validate` sums with this: a hand-edited project (a speed near zero, an
    /// extreme span) must be rejected, not panic the caller.
    pub(crate) fn checked_duration(&self) -> Option<Seconds> {
        match self {
            Segment::Clip(c) => c.checked_duration(),
            Segment::Gap(g) => Some(g.duration),
            Segment::Transition(t) => Some(t.dur),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// A muted track contributes nothing to the mix. Defaults to `false` so
    /// a track written before this field existed still parses.
    #[serde(default)]
    pub muted: bool,
    pub segments: Vec<Segment>,
}

impl Track {
    /// A new, empty track of `kind`.
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            muted: false,
            segments: Vec::new(),
        }
    }

    /// Total duration: the sum of segment durations.
    pub fn end(&self) -> Seconds {
        self.segments
            .iter()
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// Start time of the segment at `index` (prefix sum of durations).
    pub fn start_of(&self, index: usize) -> Seconds {
        self.segments
            .iter()
            .take(index)
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// [`Track::end`] with checked arithmetic: `None` when a partial sum
    /// can't be represented. A validated track is always representable, but
    /// an edit's *intermediate* track can hold durations (a one-frame
    /// transition at an extreme fps) whose exact prefix sum exceeds
    /// `Ratio<i64>` even though the final total reduces. Refuse, never panic.
    pub(crate) fn checked_end(&self) -> Option<Seconds> {
        self.segments
            .iter()
            .try_fold(Seconds::from_integer(0), |acc, s| {
                checked_add(&acc, &s.checked_duration()?)
            })
    }

    /// [`Track::start_of`] with checked arithmetic.
    pub(crate) fn checked_start_of(&self, index: usize) -> Option<Seconds> {
        self.segments
            .iter()
            .take(index)
            .try_fold(Seconds::from_integer(0), |acc, s| {
                checked_add(&acc, &s.checked_duration()?)
            })
    }

    /// Index of the segment containing `t` (start ≤ t < end).
    pub fn index_at(&self, t: Seconds) -> Option<usize> {
        let mut acc = Seconds::from_integer(0);
        for (i, s) in self.segments.iter().enumerate() {
            let end = acc + s.duration();
            if acc <= t && t < end {
                return Some(i);
            }
            acc = end;
        }
        None
    }

    /// The clip segment containing `t`, if any.
    pub fn clip_at(&self, t: Seconds) -> Option<&Clip> {
        let i = self.index_at(t)?;
        match &self.segments[i] {
            Segment::Clip(c) => Some(c),
            Segment::Gap(_) | Segment::Transition(_) => None,
        }
    }

    /// Index of the clip segment with the given id.
    pub fn clip_index(&self, id: u32) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| matches!(s, Segment::Clip(c) if c.id == id))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub tracks: Vec<Track>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// A timeline with one empty video track.
    pub fn new() -> Self {
        Self {
            tracks: vec![Track::new(TrackKind::Video)],
        }
    }

    /// Slice the whole timeline to `[start, end)`. Clips are trimmed at the
    /// source level (speed included); gaps are shortened; a transition must
    /// be wholly inside the range. Every track is padded to the range length
    /// so an export of the slice has exactly `end - start` seconds.
    pub fn slice_range(&self, start: Seconds, end: Seconds) -> Result<Timeline, TimelineError> {
        if end <= start {
            return Err(TimelineError::Corrupt("empty slice range".into()));
        }
        let length = end - start;
        let mut tracks = Vec::with_capacity(self.tracks.len());
        for track in &self.tracks {
            let mut segments = Vec::new();
            let mut acc = Seconds::from_integer(0);
            for seg in &track.segments {
                let seg_end = acc + seg.duration();
                let take_start = if start > acc { start } else { acc };
                let take_end = if end < seg_end { end } else { seg_end };
                if take_start < take_end {
                    match seg {
                        Segment::Gap(_g) => segments.push(Segment::Gap(Gap {
                            duration: take_end - take_start,
                        })),
                        Segment::Clip(c) => {
                            let mut c = c.clone();
                            // Both ends are measured from the original start.
                            let in0 = c.asset_in;
                            c.asset_in = source_time(in0, c.speed, take_start - acc);
                            c.asset_out = source_time(in0, c.speed, take_end - acc);
                            if c.asset_out > c.asset_in {
                                segments.push(Segment::Clip(c));
                            }
                        }
                        Segment::Transition(tr) => {
                            if take_start != acc || take_end != seg_end {
                                return Err(TimelineError::Corrupt(
                                    "slice cuts through a transition".into(),
                                ));
                            }
                            segments.push(Segment::Transition(tr.clone()));
                        }
                    }
                }
                acc = seg_end;
            }
            let used: Seconds = segments.iter().map(Segment::duration).sum();
            if used < length {
                segments.push(Segment::Gap(Gap {
                    duration: length - used,
                }));
            }
            tracks.push(Track {
                kind: track.kind,
                muted: track.muted,
                segments,
            });
        }
        Ok(Timeline { tracks })
    }

    /// Track at `ti` (by position — the stacking order).
    pub fn track(&self, ti: usize) -> Option<&Track> {
        self.tracks.get(ti)
    }

    pub fn track_mut(&mut self, ti: usize) -> Option<&mut Track> {
        self.tracks.get_mut(ti)
    }

    /// All video tracks, in stacking order.
    pub fn video_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Video)
    }

    /// All audio tracks, in stacking order.
    pub fn audio_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    /// Append a track of `kind`; returns its index.
    pub fn add_track(&mut self, kind: TrackKind) -> usize {
        self.tracks.push(Track::new(kind));
        self.tracks.len() - 1
    }

    /// The first video track. Editing still assumes a single video track
    /// until the ops take an explicit index (M4 step 2).
    pub fn video_track(&self) -> Option<&Track> {
        self.video_tracks().next()
    }

    pub fn video_track_mut(&mut self) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.kind == TrackKind::Video)
    }

    /// End of the timeline (across all tracks).
    pub fn end(&self) -> Seconds {
        self.tracks
            .iter()
            .map(|t| t.end())
            .max()
            .unwrap_or_else(|| Seconds::from_integer(0))
    }

    /// Remove a clip by id, searching all tracks.
    pub fn remove_clip(&mut self, id: u32) -> Result<(), TimelineError> {
        for track in &mut self.tracks {
            if let Some(i) = track.clip_index(id) {
                track.segments.remove(i);
                return Ok(());
            }
        }
        Err(TimelineError::ClipNotFound(id))
    }

    /// Structural sanity: at least one video track, positive spans and
    /// gaps. Valid asset refs are the PROJECT's job (it owns the pool).
    /// Overlaps are unrepresentable.
    pub fn validate(&self) -> Result<(), TimelineError> {
        if self.video_tracks().next().is_none() {
            return Err(TimelineError::NoVideoTrack);
        }
        let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
        for (ti, track) in self.tracks.iter().enumerate() {
            // Each segment's own invariants first: a zero `speed` must not
            // reach the length arithmetic below (it divides by it).
            for (si, seg) in track.segments.iter().enumerate() {
                let bad =
                    |what: &str| TimelineError::Corrupt(format!("track {ti} segment {si}: {what}"));
                match seg {
                    Segment::Clip(c) => {
                        if c.asset_out <= c.asset_in {
                            return Err(bad("clip has a non-positive source span"));
                        }
                        if c.speed <= Seconds::from_integer(0) {
                            return Err(bad("clip has a non-positive speed"));
                        }
                        if let Some(why) = c.effects.iter().find_map(Effect::validation_error) {
                            return Err(bad(why));
                        }
                    }
                    Segment::Gap(g) => {
                        if g.duration <= Seconds::from_integer(0) {
                            return Err(bad("gap has a non-positive duration"));
                        }
                    }
                    Segment::Transition(t) => {
                        if track.kind != TrackKind::Video {
                            return Err(bad("a transition is on an audio track"));
                        }
                        if t.dur <= Seconds::from_integer(0) {
                            return Err(bad("transition has a non-positive duration"));
                        }
                        if t.a.asset().is_none() && t.b.asset().is_none() {
                            return Err(bad("transition has no clip side"));
                        }
                        for side in [&t.a, &t.b] {
                            if let TransitionSource::Clip {
                                src_in,
                                src_out,
                                effects,
                                ..
                            } = side
                            {
                                if src_out <= src_in {
                                    return Err(bad(
                                        "transition side has a non-positive source span",
                                    ));
                                }
                                if let Some(why) = effects.iter().find_map(Effect::validation_error)
                                {
                                    return Err(bad(why));
                                }
                            }
                        }
                    }
                }
            }
            // Now the length. The durations and their sum are exact rationals,
            // so a hand-edited value that overflows either is corrupt — not
            // "long", and certainly not a panic.
            let mut end = Seconds::from_integer(0);
            for (si, seg) in track.segments.iter().enumerate() {
                let Some(d) = seg.checked_duration() else {
                    return Err(TimelineError::Corrupt(format!(
                        "track {ti} segment {si}: duration overflows"
                    )));
                };
                end = checked_add(&end, &d).ok_or_else(|| {
                    TimelineError::Corrupt(format!("track {ti}: length overflows"))
                })?;
            }
            if end > limit {
                // Not "corrupt": an edit simply asked for too much.
                return Err(TimelineError::TooLong {
                    len: format_seconds(&end),
                    limit: MAX_TIMELINE_SECONDS,
                });
            }
        }
        Ok(())
    }
}

/// Format helper re-exported for op descriptions.
#[allow(unused_imports)]
use format_seconds as _fmt_unused;

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u32, dur: i64) -> Segment {
        Segment::Clip(Clip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    }

    fn gap(dur: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(dur),
        })
    }

    #[test]
    fn non_finite_effect_parameters_are_rejected() {
        let mut t = Timeline::new();
        let mut c = match clip(1, 2) {
            Segment::Clip(c) => c,
            _ => unreachable!(),
        };
        c.effects.push(Effect::Crop {
            x: 10.0,
            y: 0.0,
            w: f64::NAN,
            h: 100.0,
            keys: Vec::new(),
        });
        t.tracks[0].segments.push(Segment::Clip(c));
        assert!(matches!(t.validate(), Err(TimelineError::Corrupt(_))));
    }

    #[test]
    fn out_of_range_effect_parameters_report_the_range() {
        let mut t = Timeline::new();
        let mut c = match clip(1, 2) {
            Segment::Clip(c) => c,
            _ => unreachable!(),
        };
        // Finite, but the window runs past the picture.
        c.effects.push(Effect::Crop {
            x: 90.0,
            y: 0.0,
            w: 50.0,
            h: 100.0,
            keys: Vec::new(),
        });
        t.tracks[0].segments.push(Segment::Clip(c));
        match t.validate() {
            Err(TimelineError::Corrupt(message)) => {
                assert!(message.contains("out of range"), "{message}");
            }
            other => panic!("expected a range refusal, got {other:?}"),
        }

        let mut t = Timeline::new();
        let mut c = match clip(1, 2) {
            Segment::Clip(c) => c,
            _ => unreachable!(),
        };
        c.effects.push(Effect::Crop {
            x: 0.0,
            y: 0.0,
            w: f64::NAN,
            h: 100.0,
            keys: Vec::new(),
        });
        t.tracks[0].segments.push(Segment::Clip(c));
        match t.validate() {
            Err(TimelineError::Corrupt(message)) => {
                assert!(message.contains("non-finite"), "{message}");
            }
            other => panic!("expected a finite refusal, got {other:?}"),
        }
    }

    #[test]
    fn crop_keys_rebase_and_scale_with_the_clip() {
        let mut c = Clip::new(1, 1, Seconds::from_integer(0), Seconds::from_integer(2));
        c.effects = vec![Effect::Crop {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
            keys: vec![
                CropKey {
                    at: Seconds::new(1, 2),
                    x: 0.0,
                    y: 0.0,
                    w: 50.0,
                    h: 50.0,
                },
                CropKey {
                    at: Seconds::new(3, 2),
                    x: 50.0,
                    y: 0.0,
                    w: 50.0,
                    h: 50.0,
                },
            ],
        }];
        c.rebase_crop_keys(Seconds::from_integer(1), Seconds::from_integer(2));
        let Effect::Crop { keys, .. } = &c.effects[0] else {
            panic!("crop");
        };
        assert_eq!(keys[0].at, Seconds::from_integer(0));
        assert_eq!(keys[0].x, 25.0, "the cut value becomes the new head");
        assert_eq!(keys[0].w, 50.0);
        assert_eq!(keys[1].at, Seconds::new(1, 2));
        assert_eq!(keys[1].x, 50.0);

        c.scale_crop_keys(Seconds::new(1, 2));
        let Effect::Crop { keys, .. } = &c.effects[0] else {
            panic!("crop");
        };
        assert_eq!(keys[0].at, Seconds::from_integer(0));
        assert_eq!(keys[1].at, Seconds::new(1, 4));

        c.truncate_crop_keys(Seconds::new(1, 8), Seconds::from_integer(2));
        let Effect::Crop { keys, .. } = &c.effects[0] else {
            panic!("crop");
        };
        assert_eq!(keys.len(), 2, "the old tail key became the cut endpoint");
        assert_eq!(keys[1].at, Seconds::new(1, 8));
        assert_eq!(keys[1].x, 37.5, "the endpoint samples the pass at the cut");
    }

    fn crop_key(at: i64, x: f64, y: f64, w: f64, h: f64) -> CropKey {
        CropKey {
            at: Seconds::from_integer(at),
            x,
            y,
            w,
            h,
        }
    }

    fn crop_effect(base: (f64, f64, f64, f64), keys: Vec<CropKey>) -> Effect {
        Effect::Crop {
            x: base.0,
            y: base.1,
            w: base.2,
            h: base.3,
            keys,
        }
    }

    fn crop_pass(
        (start, end): (i64, i64),
        (x0, y0, x1, y1): (f64, f64, f64, f64),
        (w, h): (f64, f64),
    ) -> CropPass {
        CropPass {
            start: Seconds::from_integer(start),
            end: Seconds::from_integer(end),
            x0,
            y0,
            x1,
            y1,
            w,
            h,
        }
    }

    #[test]
    fn crop_passes_cover_static_head_key_tail_and_exact_end_cases() {
        let cases = [
            (
                "no keys is one static pass",
                crop_effect((10.0, 20.0, 50.0, 60.0), Vec::new()),
                2,
                vec![crop_pass((0, 2), (10.0, 20.0, 10.0, 20.0), (50.0, 60.0))],
            ),
            (
                "a single key draws the head ramp, then holds",
                crop_effect(
                    (0.0, 0.0, 100.0, 100.0),
                    vec![crop_key(1, 10.0, 20.0, 50.0, 40.0)],
                ),
                2,
                vec![
                    crop_pass((0, 1), (0.0, 0.0, 10.0, 20.0), (100.0, 100.0)),
                    crop_pass((1, 2), (10.0, 20.0, 10.0, 20.0), (50.0, 40.0)),
                ],
            ),
            (
                "two keys after a head ramp",
                crop_effect(
                    (0.0, 0.0, 100.0, 100.0),
                    vec![
                        crop_key(1, 10.0, 10.0, 50.0, 50.0),
                        crop_key(3, 30.0, 20.0, 40.0, 30.0),
                    ],
                ),
                5,
                vec![
                    crop_pass((0, 1), (0.0, 0.0, 10.0, 10.0), (100.0, 100.0)),
                    crop_pass((1, 3), (10.0, 10.0, 30.0, 20.0), (50.0, 50.0)),
                    crop_pass((3, 5), (30.0, 20.0, 30.0, 20.0), (40.0, 30.0)),
                ],
            ),
            (
                "the last window holds to the clip end",
                crop_effect(
                    (0.0, 0.0, 100.0, 100.0),
                    vec![crop_key(1, 10.0, 20.0, 50.0, 40.0)],
                ),
                4,
                vec![
                    crop_pass((0, 1), (0.0, 0.0, 10.0, 20.0), (100.0, 100.0)),
                    crop_pass((1, 4), (10.0, 20.0, 10.0, 20.0), (50.0, 40.0)),
                ],
            ),
            (
                "a key at the exact duration only ends the head ramp",
                crop_effect(
                    (0.0, 0.0, 100.0, 100.0),
                    vec![crop_key(2, 25.0, 25.0, 50.0, 50.0)],
                ),
                2,
                vec![crop_pass((0, 2), (0.0, 0.0, 25.0, 25.0), (100.0, 100.0))],
            ),
            (
                "a lone past-end key leaves the base held",
                crop_effect(
                    (0.0, 0.0, 100.0, 100.0),
                    vec![crop_key(3, 25.0, 25.0, 50.0, 50.0)],
                ),
                2,
                vec![crop_pass((0, 2), (0.0, 0.0, 0.0, 0.0), (100.0, 100.0))],
            ),
            (
                "a past-end key endpoints the final pass",
                crop_effect(
                    (0.0, 25.0, 20.0, 20.0),
                    vec![
                        crop_key(1, 0.0, 25.0, 20.0, 20.0),
                        crop_key(20, 70.0, 25.0, 20.0, 20.0),
                    ],
                ),
                10,
                vec![
                    crop_pass((0, 1), (0.0, 25.0, 0.0, 25.0), (20.0, 20.0)),
                    crop_pass((1, 10), (0.0, 25.0, 70.0, 25.0), (20.0, 20.0)),
                ],
            ),
        ];
        for (name, effect, duration, expected) in cases {
            assert_eq!(
                crop_passes(&effect, Seconds::from_integer(duration)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn crop_pass_windows_clamp_at_their_ends() {
        let pass = crop_pass((1, 3), (0.0, 10.0, 40.0, 30.0), (50.0, 25.0));
        assert_eq!(
            pass.window_at(Seconds::from_integer(0)),
            CropWindow {
                x: 0.0,
                y: 10.0,
                w: 50.0,
                h: 25.0,
            }
        );
        assert_eq!(
            pass.window_at(Seconds::from_integer(2)),
            CropWindow {
                x: 20.0,
                y: 20.0,
                w: 50.0,
                h: 25.0,
            }
        );
        assert_eq!(
            pass.window_at(Seconds::from_integer(4)),
            CropWindow {
                x: 40.0,
                y: 30.0,
                w: 50.0,
                h: 25.0,
            }
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "crop key before the clip head")]
    fn crop_passes_debug_assert_on_a_negative_key() {
        let effect = crop_effect(
            (0.0, 0.0, 100.0, 100.0),
            vec![
                crop_key(-1, 10.0, 10.0, 50.0, 50.0),
                crop_key(1, 20.0, 20.0, 40.0, 40.0),
            ],
        );
        let _ = crop_passes(&effect, Seconds::from_integer(2));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "crop keys must be strictly increasing")]
    fn crop_passes_debug_assert_on_unsorted_keys() {
        let effect = crop_effect(
            (0.0, 0.0, 100.0, 100.0),
            vec![
                crop_key(2, 10.0, 10.0, 50.0, 50.0),
                crop_key(1, 20.0, 20.0, 40.0, 40.0),
            ],
        );
        let _ = crop_passes(&effect, Seconds::from_integer(3));
    }

    #[test]
    fn crop_passes_skip_a_negative_key_without_the_debug_assert() {
        // Call the plan helper directly: this test must pass in a release
        // build too, where the public planner's debug assertion is gone.
        let keys = [
            crop_key(-1, 10.0, 10.0, 50.0, 50.0),
            crop_key(1, 20.0, 20.0, 40.0, 40.0),
            crop_key(2, 30.0, 30.0, 30.0, 30.0),
        ];
        let refs: Vec<&CropKey> = keys.iter().collect();
        let passes = keyed_passes(
            CropWindow {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            },
            &refs,
            Seconds::from_integer(2),
        );
        assert_eq!(
            passes,
            vec![crop_pass((1, 2), (20.0, 20.0, 30.0, 30.0), (40.0, 40.0))]
        );
        assert!(
            passes
                .iter()
                .all(|pass| pass.start >= Seconds::from_integer(0))
        );
    }

    #[test]
    fn head_trim_rebases_to_the_rendered_tail_key_window() {
        let effect = crop_effect(
            (0.0, 25.0, 20.0, 20.0),
            vec![
                crop_key(1, 0.0, 25.0, 20.0, 20.0),
                crop_key(20, 70.0, 25.0, 20.0, 20.0),
            ],
        );
        // The pre-trim clip is [0, 10) with a key past its end. A head trim
        // at 5 leaves `asset_in = 5` (duration 5) and only then rebases.
        let rendered = crop_passes(&effect, Seconds::from_integer(10))
            .iter()
            .rev()
            .find(|pass| pass.start <= Seconds::from_integer(5))
            .expect("a pass covers the trim point")
            .window_at(Seconds::from_integer(5));
        assert!(
            (rendered.x - 280.0 / 9.0).abs() < 1e-9,
            "the renderer interpolates key 1→20 over [1, 10]: {rendered:?}"
        );

        let mut clip = Clip::new(1, 1, Seconds::from_integer(5), Seconds::from_integer(10));
        clip.effects = vec![effect.clone()];
        clip.rebase_crop_keys(Seconds::from_integer(5), Seconds::from_integer(10));
        let Effect::Crop { keys, .. } = &clip.effects[0] else {
            panic!("crop");
        };
        let head = keys
            .iter()
            .find(|key| key.at == Seconds::from_integer(0))
            .expect("the cut value becomes the head key");
        assert_eq!(head.x, rendered.x, "the rebase samples the pass plan");
        assert_eq!(head.y, rendered.y);
        assert_eq!(head.w, rendered.w);
        assert_eq!(head.h, rendered.h);
    }

    #[test]
    fn slicing_close_large_denominators_stays_valid() {
        let mut t = Timeline::new();
        let mut c = match clip(1, 2) {
            Segment::Clip(c) => c,
            _ => unreachable!(),
        };
        c.asset_in = Seconds::new(299, 15);
        c.asset_out = Seconds::from_integer(20);
        c.speed = Seconds::new(1, 1_000_000);
        t.tracks[0].segments.push(Segment::Clip(c));
        let sliced = t
            .slice_range(Seconds::new(57_273, 1000), Seconds::new(59_202, 1000))
            .unwrap();
        assert_eq!(sliced.end(), Seconds::new(1_929, 1000));
        sliced.validate().unwrap();
    }

    #[test]
    fn source_mapping_scales_by_speed() {
        // 2× clip reading source [1, 5): 1s of timeline = 2s of source.
        let c = Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        };
        assert_eq!(
            c.source_at(Seconds::from_integer(1)),
            Seconds::from_integer(3)
        );
        assert_eq!(
            c.timeline_offset_of(Seconds::from_integer(3)),
            Seconds::from_integer(1)
        );
    }

    #[test]
    fn end_is_the_sum_of_segments() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.end(), Seconds::from_integer(6));
        assert_eq!(t.start_of(0), Seconds::from_integer(0));
        assert_eq!(t.start_of(1), Seconds::from_integer(2));
        assert_eq!(t.start_of(2), Seconds::from_integer(3));
        assert_eq!(t.start_of(3), Seconds::from_integer(6));
    }

    #[test]
    fn index_at_and_clip_at() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.index_at(Seconds::from_integer(0)), Some(0));
        assert_eq!(t.index_at(Seconds::from_integer(2)), Some(1), "gap start");
        assert_eq!(
            t.index_at(Seconds::from_integer(3)),
            Some(2),
            "the clip after the gap"
        );
        assert_eq!(
            t.index_at(Seconds::from_integer(6)),
            None,
            "the end belongs to no segment"
        );
        assert!(t.clip_at(Seconds::from_integer(4)).is_some());
        assert!(
            t.clip_at(Seconds::from_integer(2)).is_none(),
            "inside a gap"
        );
        assert_eq!(t.clip_index(2), Some(2));
        assert_eq!(t.clip_index(9), None);
    }

    #[test]
    fn validate_rejects_bad_spans() {
        let mut p_timeline = Timeline::new();
        p_timeline.tracks[0].segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(2),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        assert!(p_timeline.validate().is_err());
        p_timeline.tracks[0].segments = vec![gap(0)];
        assert!(p_timeline.validate().is_err());
    }

    #[test]
    fn tracks_are_addressed_by_index_and_kind() {
        let mut t = Timeline::new();
        assert_eq!(t.tracks.len(), 1, "one video track to start");
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 0);
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        assert!(t.track(1).is_none());

        let ai = t.add_track(TrackKind::Audio);
        assert_eq!(ai, 1, "add_track returns the new index");
        assert_eq!(t.track(1).unwrap().kind, TrackKind::Audio);
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 1);
        // video_track() is the *first* video, not the last track.
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        t.track_mut(1).unwrap().muted = true;
        assert!(t.track(1).unwrap().muted);
    }

    #[test]
    fn muted_defaults_false_and_round_trips() {
        let t = Track::new(TrackKind::Audio);
        assert!(!t.muted, "new tracks are not muted");
        // A track written before the field existed parses as unmuted.
        let legacy: Track = serde_json::from_str(r#"{"kind":"Video","segments":[]}"#).unwrap();
        assert!(!legacy.muted);
        // And it round-trips once set.
        let mut t = Track::new(TrackKind::Video);
        t.muted = true;
        let json = serde_json::to_string(&t).unwrap();
        let back: Track = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn validate_requires_a_video_track() {
        let no_video = Timeline {
            tracks: vec![Track::new(TrackKind::Audio)],
        };
        assert!(matches!(
            no_video.validate(),
            Err(TimelineError::NoVideoTrack)
        ));

        let mut ok = Timeline::new();
        ok.add_track(TrackKind::Audio);
        assert!(ok.validate().is_ok());
    }

    fn trans(dur: i64, a: TransitionSource) -> Segment {
        Segment::Transition(Transition {
            id: 1,
            dur: Seconds::from_integer(dur),
            kind: TransitionKind::Dissolve,
            a,
            b: TransitionSource::Black,
        })
    }

    #[test]
    fn validate_rejects_bad_transitions() {
        let clip = |in_: i64, out: i64| TransitionSource::Clip {
            asset: 1,
            src_in: Seconds::from_integer(in_),
            src_out: Seconds::from_integer(out),
            effects: Vec::new(),
        };
        let mut t = Timeline::new();

        t.tracks[0].segments = vec![trans(1, TransitionSource::Black)];
        assert!(t.validate().is_err(), "needs a clip side");

        t.tracks[0].segments = vec![trans(0, clip(0, 1))];
        assert!(t.validate().is_err(), "non-positive duration");

        t.tracks[0].segments = vec![trans(1, clip(2, 2))];
        assert!(t.validate().is_err(), "non-positive source span");

        t.tracks[0].segments = vec![trans(1, clip(0, 1))];
        assert!(t.validate().is_ok());

        // A transition on an audio track can't render (the exporter's graph
        // is video-only for it) — the loader refuses the hand-edited state
        // the edit paths already forbid.
        let mut t = Timeline::new();
        t.add_track(TrackKind::Audio);
        t.tracks[1].segments = vec![trans(1, clip(0, 1))];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("audio")),
            "{:?}",
            t.validate()
        );
    }

    /// Hand-edited arithmetic that the rational can't hold is rejected, not a
    /// panic: `validate` used to sum durations before checking them (a zero
    /// `speed` divided by zero) and to add with the panicking operator.
    #[test]
    fn validate_rejects_arithmetic_it_cannot_hold() {
        let mut t = Timeline::new();
        let clip = |id, out: Seconds, speed: Seconds| {
            Segment::Clip(Clip {
                id,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: out,
                speed,
                effects: Vec::new(),
            })
        };
        let one = Seconds::from_integer(1);
        let zero = Seconds::from_integer(0);

        t.tracks[0].segments = vec![clip(1, Seconds::from_integer(3), zero)];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("speed")),
            "a zero speed is caught before the length sums"
        );

        // Two tiny slivers whose exact sum overflows the numerator.
        t.tracks[0].segments = vec![
            clip(1, Seconds::new(1, i64::MAX), one),
            clip(2, Seconds::new(1, i64::MAX - 1), one),
        ];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("overflows")),
            "an overflowing sum is corrupt, not a panic"
        );

        // A single duration that can't be represented at all.
        t.tracks[0].segments = vec![clip(1, Seconds::from_integer(3), Seconds::new(1, i64::MAX))];
        assert!(
            matches!(t.validate(), Err(TimelineError::Corrupt(ref m)) if m.contains("overflows")),
            "an overflowing duration is corrupt, not a panic"
        );
    }

    // One definition of the timeline↔source boundary: a retimed clip's two
    // conversions are inverses, and the free functions are what both a `Clip`
    // and a recorded `FrameSource` call.
    #[test]
    fn the_source_boundary_round_trips_at_a_retime() {
        let c = Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(3),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        };
        let offset = Seconds::new(1, 2); // half a timeline second at 2×
        let source = c.source_at(offset);
        assert_eq!(source, Seconds::from_integer(4), "3 s + 0.5 s × 2");
        assert_eq!(c.timeline_offset_of(source), offset, "the inverse");
        assert_eq!(source_time(c.asset_in, c.speed, offset), source);
        assert_eq!(
            c.duration(),
            Seconds::from_integer(1),
            "2 s of source at 2×"
        );
    }

    #[test]
    fn a_lut_path_survives_the_option_and_graph_parsers() {
        let chain = |path: &str| Effect::chain(&[Effect::Lut { path: path.into() }], (0, 0));
        // `:` splits the option list, so the option parser needs a backslash
        // even though the graph is quoted; `,` is graph-level only.
        assert_eq!(
            chain("/luts/a,b:c.cube"),
            "lut3d=file='/luts/a,b\\:c.cube'",
            "a colon is escaped for the option parser, a comma by the quotes"
        );
        // A quote needs both levels: `\'` for the option parser, then `'\''`
        // for the graph.
        assert_eq!(
            chain("/luts/it's.cube"),
            r"lut3d=file='/luts/it\'\''s.cube'",
            "a quote is escaped at both levels"
        );
        // The option parser reads `\` as an escape even inside the quotes:
        // one literal backslash reaches it as two, and a trailing one must
        // not eat the closing quote.
        assert_eq!(
            chain(r"C:\luts\film.cube"),
            r"lut3d=file='C\:\\luts\\film.cube'",
            "a backslash is doubled and the drive colon escaped"
        );
        assert_eq!(
            chain(r"C:\luts\"),
            r"lut3d=file='C\:\\luts\\'",
            "a trailing backslash cannot escape the quote"
        );
    }

    // The geometry a check reads and the filters the exporter renders are the
    // same tracker: the final `pad` must place the picture exactly where
    // `visible`/`project` say it is.
    #[test]
    fn effect_geometry_agrees_with_the_chain_it_renders() {
        let effects = vec![
            Effect::Crop {
                x: 10.0,
                y: 10.0,
                w: 80.0,
                h: 80.0,
                keys: Vec::new(),
            },
            Effect::Scale { w: 50.0, h: 50.0 },
            Effect::Offset { x: 10.0, y: 0.0 },
        ];
        let canvas = (1920, 1080);
        let geometry = Effect::geometry(&effects, canvas);
        assert_eq!(geometry.visible(), (710, 324, 768, 432));
        // The crop's source rect (10..90% each axis) lands on that same box.
        assert_eq!(
            geometry.project(0.1, 0.1, 0.8, 0.8),
            (710.0, 324.0, 768.0, 432.0)
        );
        assert!(geometry.changes_picture());
        let chain = Effect::chain(&effects, canvas);
        assert!(
            chain.ends_with("pad=1920:1080:710:324:color=black@0"),
            "the export pads the visible box it reports: {chain}"
        );
    }

    #[test]
    fn keyed_crop_geometry_is_only_its_static_base() {
        // `Effect::geometry` models the static chain; `Effect::chain` itself
        // does not refuse keys (the exporter's `effect_chain` does). Pin that
        // so a caller cannot mistake the static base for the keyed window.
        let effects = vec![Effect::Crop {
            x: 10.0,
            y: 10.0,
            w: 80.0,
            h: 80.0,
            keys: vec![CropKey {
                at: Seconds::from_integer(0),
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            }],
        }];
        let geometry = Effect::geometry(&effects, (1920, 1080));
        assert_eq!(geometry.visible(), (192, 108, 1536, 864));
        assert!(geometry.changes_picture());
    }

    #[test]
    fn no_op_geometry_stays_identity() {
        let canvas = (1920, 1080);
        for effects in [
            Vec::new(),
            vec![Effect::Scale { w: 100.0, h: 100.0 }],
            vec![Effect::Crop {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
                keys: Vec::new(),
            }],
        ] {
            let geometry = Effect::geometry(&effects, canvas);
            assert_eq!(geometry.visible(), (0, 0, 1920, 1080));
            assert!(!geometry.changes_picture(), "{effects:?}");
            assert_eq!(
                geometry.project(0.25, 0.5, 0.5, 0.5),
                (480.0, 540.0, 960.0, 540.0)
            );
        }
    }

    #[test]
    fn zoom_and_offset_clip_the_visible_box_to_the_canvas() {
        let canvas = (1920, 1080);
        // A 200% centre zoom keeps the full canvas but maps the source wider
        // than it: the picture is cut on both sides.
        let zoom = Effect::geometry(&[Effect::Scale { w: 200.0, h: 200.0 }], canvas);
        assert_eq!(zoom.visible(), (0, 0, 1920, 1080));
        assert!(zoom.changes_picture());
        assert_eq!(
            zoom.project(0.0, 0.0, 1.0, 1.0),
            (-960.0, -540.0, 3840.0, 2160.0)
        );

        let off = Effect::geometry(&[Effect::Offset { x: 100.0, y: 0.0 }], canvas);
        assert!(off.is_empty());
        assert!(off.changes_picture());
    }
}
