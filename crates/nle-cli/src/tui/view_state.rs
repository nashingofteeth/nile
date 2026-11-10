//! The timeline session sidecar: viewport zoom/scroll, the active track, and
//! pool source marks, beside the project under `.nile/<stem>/view.json`.
//!
//! This is convenience state, like the yank register: a missing, corrupt or
//! version-skewed file simply means "fit the timeline on the first track".

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::PoolMarks;
use super::motion::TimelineView;

pub(crate) const VIEW_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SessionState {
    pub view: TimelineView,
    pub active_track: usize,
    pub pool_marks: BTreeMap<u32, PoolMarks>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ViewFile {
    version: u32,
    zoom: f64,
    start: f64,
    active_track: usize,
    #[serde(default)]
    pool_marks: BTreeMap<u32, PoolMarks>,
}

/// `nile.json` → `.nile/nile/view.json`.
pub(crate) fn path(project_path: &Path) -> PathBuf {
    nle_core::state::state_dir(project_path).join("view.json")
}

pub(crate) fn load(project_path: &Path) -> SessionState {
    let Ok(text) = std::fs::read_to_string(path(project_path)) else {
        return SessionState::default();
    };
    let Ok(file) = serde_json::from_str::<ViewFile>(&text) else {
        return SessionState::default();
    };
    if file.version != VIEW_VERSION
        || !file.zoom.is_finite()
        || file.zoom < 1.0
        || !file.start.is_finite()
        || file.start < 0.0
    {
        return SessionState::default();
    }
    let pool_marks = file
        .pool_marks
        .into_iter()
        .filter(|(_, marks)| valid_marks(*marks))
        .collect();
    SessionState {
        view: TimelineView {
            zoom: file.zoom,
            start: file.start,
        },
        active_track: file.active_track,
        pool_marks,
    }
}

/// Atomic save, like the project and history sidecars.
pub(crate) fn save(project_path: &Path, state: SessionState) -> io::Result<()> {
    let file = ViewFile {
        version: VIEW_VERSION,
        zoom: if state.view.zoom.is_finite() {
            state.view.zoom.max(1.0)
        } else {
            1.0
        },
        start: if state.view.start.is_finite() {
            state.view.start.max(0.0)
        } else {
            0.0
        },
        active_track: state.active_track,
        pool_marks: state.pool_marks,
    };
    let path = path(project_path);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut json = serde_json::to_string_pretty(&file)
        .map_err(|e| io::Error::other(format!("view serialize: {e}")))?;
    json.push('\n');
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn valid_marks(marks: PoolMarks) -> bool {
    let valid = |t: Option<f64>| t.is_none_or(|t| t.is_finite() && t >= 0.0);
    valid(marks.in_at)
        && valid(marks.out_at)
        && !matches!((marks.in_at, marks.out_at), (Some(a), Some(b)) if a >= b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_round_trips_through_its_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("nile.json");
        assert_eq!(load(&project), SessionState::default(), "no file, fit");
        let state = SessionState {
            view: TimelineView {
                zoom: 3.5,
                start: 12.25,
            },
            active_track: 2,
            pool_marks: BTreeMap::from([(
                7,
                PoolMarks {
                    in_at: Some(1.5),
                    out_at: Some(4.0),
                },
            )]),
        };
        save(&project, state.clone()).unwrap();
        assert_eq!(load(&project), state);
        assert!(path(&project).exists());
    }

    #[test]
    fn corrupt_or_skewed_state_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("nile.json");
        let path = path(&project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&project), SessionState::default());

        std::fs::write(
            &path,
            r#"{"version":99,"zoom":2.0,"start":1.0,"active_track":1}"#,
        )
        .unwrap();
        assert_eq!(load(&project), SessionState::default());

        std::fs::write(
            &path,
            r#"{"version":3,"zoom":0.5,"start":1.0,"active_track":1}"#,
        )
        .unwrap();
        assert_eq!(load(&project), SessionState::default());

        // Malformed or inverted marks are dropped, not applied to an asset.
        std::fs::write(
            &path,
            r#"{"version":3,"zoom":1.0,"start":0.0,"active_track":0,"pool_marks":{"7":{"in_at":4.0,"out_at":2.0},"8":{"in_at":1.0,"out_at":2.0}}}"#,
        )
        .unwrap();
        let state = load(&project);
        assert_eq!(state.pool_marks.len(), 1);
        assert_eq!(
            state.pool_marks.get(&8),
            Some(&PoolMarks {
                in_at: Some(1.0),
                out_at: Some(2.0)
            })
        );
    }
}
