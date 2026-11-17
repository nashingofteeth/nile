//! Notes, markers and (later) expectations anchored to source media.
//!
//! A source anchor is a media time, not a timeline position: trimming,
//! splitting or rippling the timeline cannot move it. Timeline anchors are a
//! later slice and will be clip-relative so edits can re-anchor them.

use serde::{Deserialize, Serialize};

use crate::provenance::Provenance;
use crate::time::{Seconds, rational_ser};

/// A note or marker attached to a project sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: u32,
    pub sequence: u32,
    pub kind: AnnotationKind,
    pub anchor: Anchor,
    pub provenance: Provenance,
    #[serde(default)]
    pub status: AnnotationStatus,
}

impl Annotation {
    /// The note text, if this is a note.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            AnnotationKind::Note { text } => Some(text),
            AnnotationKind::Marker => None,
        }
    }

    /// A short label for notices and lists.
    pub fn label(&self) -> &'static str {
        match self.kind {
            AnnotationKind::Note { .. } => "note",
            AnnotationKind::Marker => "marker",
        }
    }
}

/// What the annotation says. The payload lives in the variant, so a note
/// cannot exist without text and a marker cannot carry stray text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnnotationKind {
    Note { text: String },
    Marker,
}

/// Where the annotation points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Anchor {
    /// A time inside one source file, or inside a logical recording's group
    /// clock. Both survive timeline edits.
    Source {
        source: SourceRef,
        #[serde(with = "rational_ser")]
        time: Seconds,
    },
}

/// Which media clock a source anchor uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum SourceRef {
    Asset { id: u32 },
    Group { id: u32 },
}

/// Resolution state for directives and expectations; notes can be resolved
/// too, so deleting is not the only way to clear a review list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationStatus {
    #[default]
    Open,
    Resolved,
}
