//! The yank register: one or more segments plus, for clips, the asset paths
//! they came from, in a sidecar beside the project. Convenience state — a
//! missing or version-skewed file reads as empty.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::edit::EditError;
use crate::project::Project;
use crate::timeline::Segment;

/// Register sidecar format version. Bumped on a shape change; a skewed or
/// unreadable file reads as an empty register (the register is
/// convenience, not critical state — the project file is).
/// v3 carries a sequence of entries (single yanks are a one-entry range).
pub const REGISTER_VERSION: u32 = 3;

/// One yanked segment plus, for a clip, the asset path it came from. Asset
/// ids are **project-scoped**, so a register that outlives its project
/// (a rebuilt project, a copied sidecar) must be resolved against the
/// pool by path — pasting the wrong media silently would be worse than
/// refusing.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisterEntry {
    pub segment: Segment,
    pub path: Option<PathBuf>,
}

impl RegisterEntry {
    pub fn of(project: &Project, segment: Segment) -> Self {
        let path = match &segment {
            Segment::Clip(c) => project.asset(c.asset).map(|a| a.path.clone()),
            Segment::Gap(_) => None,
            // A transition carries its own asset ids; resolving those across
            // projects is out of scope (register paste of a transition).
            Segment::Transition(_) => None,
        };
        Self { segment, path }
    }

    /// Resolve against `project`: same file, possibly under a different
    /// asset id. Refuses when the media isn't in this pool at all.
    pub fn resolve(&self, project: &Project) -> Result<Segment, EditError> {
        let Segment::Clip(c) = &self.segment else {
            return Ok(self.segment.clone());
        };
        let Some(path) = &self.path else {
            return Ok(self.segment.clone());
        };
        if project.asset(c.asset).map(|a| &a.path) == Some(path) {
            return Ok(self.segment.clone()); // same id, same file
        }
        let Some(asset) = project.media.iter().find(|a| &a.path == path) else {
            return Err(EditError::RegisterForeign(path.display().to_string()));
        };
        let mut segment = self.segment.clone();
        if let Segment::Clip(clip) = &mut segment {
            clip.asset = asset.id;
        }
        Ok(segment)
    }
}

/// A yank register: a single segment or a contiguous visual range.
#[derive(Debug, Clone, PartialEq)]
pub struct Register {
    pub entries: Vec<RegisterEntry>,
}

impl Register {
    /// A one-segment register (the `yy` shape).
    pub fn of(project: &Project, segment: Segment) -> Self {
        Self::of_range(project, &[segment])
    }

    /// A visual range; each clip keeps the path it came from.
    pub fn of_range(project: &Project, segments: &[Segment]) -> Self {
        Self {
            entries: segments
                .iter()
                .map(|segment| RegisterEntry::of(project, segment.clone()))
                .collect(),
        }
    }

    /// Resolve a single-entry register. A range must use
    /// [`Self::resolve_range`]; pasting it as one segment would silently drop
    /// the rest.
    pub fn resolve(&self, project: &Project) -> Result<Segment, EditError> {
        match self.entries.as_slice() {
            [] => Err(EditError::RegisterEmpty),
            [entry] => entry.resolve(project),
            _ => Err(EditError::RangeRegister),
        }
    }

    /// Resolve every entry, failing if any media is foreign.
    pub fn resolve_range(&self, project: &Project) -> Result<Vec<Segment>, EditError> {
        self.entries
            .iter()
            .map(|entry| entry.resolve(project))
            .collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterFile {
    version: u32,
    entries: Vec<RegisterEntryFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterEntryFile {
    segment: Segment,
    #[serde(default)]
    path: Option<PathBuf>,
}

/// The register sidecar for a project (`nile.json` →
/// `.nile/nile/clipboard.json`).
pub fn register_path(project_path: &Path) -> PathBuf {
    crate::state::clipboard_path(project_path)
}

/// The yanked segments a project's register holds, if any. A legacy adjacent
/// sidecar is copied into the state directory on first read.
pub fn load_register(project_path: &Path) -> Option<Register> {
    load_register_with(project_path, true)
}

/// Like [`load_register`], but reads a legacy sidecar without copying it into
/// the state directory. Diagnostics (`edit --dump`) use this so inspecting a
/// project never writes to it.
pub fn load_register_without_migration(project_path: &Path) -> Option<Register> {
    load_register_with(project_path, false)
}

fn load_register_with(project_path: &Path, migrate: bool) -> Option<Register> {
    let path = register_path(project_path);
    let mut legacy = None;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let old = crate::state::legacy_clipboard_path(project_path);
            let text = std::fs::read_to_string(&old).ok()?;
            legacy = Some(old);
            text
        }
        Err(_) => return None,
    };
    let file: RegisterFile = serde_json::from_str(&text).ok()?;
    if file.version != REGISTER_VERSION || file.entries.is_empty() {
        return None;
    }
    if migrate && let Some(old) = legacy {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(old, &path);
    }
    Some(Register {
        entries: file
            .entries
            .into_iter()
            .map(|entry| RegisterEntry {
                segment: entry.segment,
                path: entry.path,
            })
            .collect(),
    })
}

/// Persist the register; `None` (or an empty range) clears it, and the file
/// goes away. Atomic, like the project save — a crash can't leave half a
/// register.
pub fn save_register(project_path: &Path, register: Option<&Register>) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = register_path(project_path);
    let Some(register) = register.filter(|register| !register.entries.is_empty()) else {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::state::legacy_clipboard_path(project_path));
        return Ok(());
    };
    let file = RegisterFile {
        version: REGISTER_VERSION,
        entries: register
            .entries
            .iter()
            .map(|entry| RegisterEntryFile {
                segment: entry.segment.clone(),
                path: entry.path.clone(),
            })
            .collect(),
    };
    let mut json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::other(format!("register serialize: {e}")))?;
    json.push('\n');
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
