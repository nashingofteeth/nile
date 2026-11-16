//! Logical recordings assembled from one or more physical files.
//!
//! A camera or microphone that rolls over into several files is one
//! `MediaGroup`: its parts have a position on the logical group clock and an
//! offset into the asset file. Group time is exact rational seconds and the
//! mapping is half-open, just like [`crate::TimeMap`].

use serde::{Deserialize, Serialize};

use crate::time::{Seconds, checked_add, checked_sub, rational_ser};

/// One file's place in a logical recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPart {
    pub asset: u32,
    /// Start on the logical group timeline.
    #[serde(with = "rational_ser")]
    pub group_start: Seconds,
    /// Start inside the asset file.
    #[serde(with = "rational_ser")]
    pub file_start: Seconds,
    #[serde(with = "rational_ser")]
    pub duration: Seconds,
}

impl MediaPart {
    pub fn group_end(&self) -> Option<Seconds> {
        checked_add(&self.group_start, &self.duration)
    }

    pub fn file_end(&self) -> Option<Seconds> {
        checked_add(&self.file_start, &self.duration)
    }
}

/// An ordered logical recording made of media parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaGroup {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parts: Vec<MediaPart>,
}

impl MediaGroup {
    pub fn new(id: u32, name: impl Into<String>, parts: Vec<MediaPart>) -> Self {
        Self {
            id,
            name: name.into(),
            parts,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// End of the last part on the group clock.
    pub fn end(&self) -> Option<Seconds> {
        self.parts.last()?.group_end()
    }

    /// Group time → `(asset, file time)`, or `None` in a gap between parts.
    pub fn group_time(&self, at: &Seconds) -> Option<(u32, Seconds)> {
        let part = self.parts.iter().find(|part| {
            part.group_start <= *at && part.group_end().is_some_and(|end| *at < end)
        })?;
        let offset = checked_sub(at, &part.group_start)?;
        let file = checked_add(&part.file_start, &offset)?;
        Some((part.asset, file))
    }

    /// `(asset, file time)` → group time, or `None` when no part reads it.
    /// If an asset appears in more than one part, the first part wins.
    pub fn media_time(&self, asset: u32, file: &Seconds) -> Option<Seconds> {
        let part = self.parts.iter().find(|part| {
            part.asset == asset
                && part.file_start <= *file
                && *file < part.file_end().unwrap_or(*file)
        })?;
        let offset = checked_sub(file, &part.file_start)?;
        checked_add(&part.group_start, &offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: i64) -> Seconds {
        Seconds::from_integer(n)
    }

    fn part(asset: u32, group: i64, file: i64, duration: i64) -> MediaPart {
        MediaPart {
            asset,
            group_start: s(group),
            file_start: s(file),
            duration: s(duration),
        }
    }

    #[test]
    fn group_time_and_media_time_round_trip() {
        let group = MediaGroup::new(1, "CAM 3", vec![part(1, 0, 10, 4), part(2, 5, 20, 3)]);
        assert_eq!(group.end(), Some(s(8)));
        assert_eq!(group.group_time(&s(0)), Some((1, s(10))));
        assert_eq!(group.group_time(&s(3)), Some((1, s(13))));
        assert_eq!(group.group_time(&s(4)), None, "gap between parts");
        assert_eq!(group.group_time(&s(5)), Some((2, s(20))));
        assert_eq!(group.media_time(1, &s(12)), Some(s(2)));
        assert_eq!(group.media_time(2, &s(22)), Some(s(7)));
        assert_eq!(group.media_time(1, &s(14)), None, "end is exclusive");
    }

    #[test]
    fn an_asset_reused_resolves_to_its_first_part() {
        let group = MediaGroup::new(1, "MIC 1", vec![part(1, 0, 0, 2), part(1, 2, 5, 2)]);
        assert_eq!(group.media_time(1, &s(5)), Some(s(2)));
        assert_eq!(group.media_time(1, &s(6)), Some(s(3)));
    }
}
