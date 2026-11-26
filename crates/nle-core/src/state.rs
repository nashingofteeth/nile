//! Per-project state paths.
//!
//! A project keeps its history, clipboard, and disposable render cache in a
//! hidden `.nile/<project-stem>/` directory beside the project file. Keeping
//! them together makes one project self-contained and lets the proxy cache be
//! pruned per project instead of globally.
//!
//! The directory is keyed on the project's **real** path: a symlinked project
//! file and its target share one history, clipboard and socket. A project that
//! does not exist yet (`init`) keeps the literal typed path until it does.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_DIR: &str = ".nile";
pub const CONFIG_FILE: &str = "config.json";

/// Directory holding all state for one project:
/// `foo/nile.json` → `foo/.nile/nile/`.
///
/// An existing path resolves to its absolute real path first, so relative and
/// symlinked spellings (`alias.json → foo/nile.json`) all land in
/// `foo/.nile/nile/`. A path that does not exist yet — `init`, synthetic
/// callers — keeps the literal `<parent>/.nile/<stem>`.
pub fn state_dir(project_path: &Path) -> PathBuf {
    let real = std::fs::canonicalize(project_path).ok();
    let project_path = real.as_deref().unwrap_or(project_path);
    let parent = project_path.parent().filter(|p| !p.as_os_str().is_empty());
    let stem = project_path
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("project"));
    match parent {
        Some(parent) => parent.join(STATE_DIR).join(stem),
        None => PathBuf::from(STATE_DIR).join(stem),
    }
}

pub fn history_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("history.json")
}

pub fn clipboard_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("clipboard.json")
}

pub fn cache_dir(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("cache")
}

/// Durable render jobs and their project snapshots.
pub fn jobs_dir(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("jobs")
}

/// Unix socket the live protocol server listens on. `sun_path` caps the whole
/// path at roughly 108 bytes on Linux, so a very deep project tree fails at
/// bind with a clear "file name too long" error.
pub fn session_socket_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("session.sock")
}

/// Startup lock beside the socket. It serializes stale-socket cleanup and
/// bind; a killed server leaves it behind for the next start to recognize.
pub fn session_lock_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("serve.lock")
}

/// Directory-level config (`.nile/config.json` in the current directory).
pub fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(STATE_DIR).join(CONFIG_FILE)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_project: Option<PathBuf>,
}

/// The directory's configured default project, if any.
pub fn load_default_project(cwd: &Path) -> std::io::Result<Option<PathBuf>> {
    let text = match std::fs::read_to_string(config_path(cwd)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let config: Config = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::other(format!("corrupt .nile/config.json: {e}")))?;
    Ok(config.default_project.filter(|p| !p.as_os_str().is_empty()))
}

/// Set or clear the directory's default project. The path is stored as
/// given; a relative path is relative to this directory.
pub fn save_default_project(cwd: &Path, project: Option<&Path>) -> std::io::Result<()> {
    let path = config_path(cwd);
    let Some(project) = project else {
        let _ = std::fs::remove_file(path);
        return Ok(());
    };
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut json = serde_json::to_string_pretty(&Config {
        default_project: Some(project.to_path_buf()),
    })
    .map_err(|e| std::io::Error::other(format!("config serialize: {e}")))?;
    json.push('\n');
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Pre-state-directory sidecar locations, used for a one-time copy.
pub(crate) fn legacy_history_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("history.json")
}

pub(crate) fn legacy_clipboard_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("clipboard.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_lives_beside_the_project_in_a_stem_directory() {
        let p = Path::new("/videos/trip/nile.json");
        assert_eq!(state_dir(p), PathBuf::from("/videos/trip/.nile/nile"));
        assert_eq!(
            history_path(p),
            PathBuf::from("/videos/trip/.nile/nile/history.json")
        );
        assert_eq!(
            clipboard_path(p),
            PathBuf::from("/videos/trip/.nile/nile/clipboard.json")
        );
        assert_eq!(cache_dir(p), PathBuf::from("/videos/trip/.nile/nile/cache"));
        assert_eq!(
            session_socket_path(p),
            PathBuf::from("/videos/trip/.nile/nile/session.sock")
        );
        assert_eq!(
            session_lock_path(p),
            PathBuf::from("/videos/trip/.nile/nile/serve.lock")
        );
    }

    #[test]
    fn a_bare_project_name_uses_the_current_directory() {
        assert_eq!(
            history_path(Path::new("nile.json")),
            PathBuf::from(".nile/nile/history.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_project_file_shares_the_real_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("nile.json");
        std::fs::write(&real, "{}").unwrap();
        let alias = dir.path().join("alias.json");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert_eq!(state_dir(&alias), state_dir(&real));
        // `tempfile` may hand back /var/... where the real path is /private/var
        // (macOS), so compare against the canonical parent, not `dir.path()`.
        let canonical_real_dir = std::fs::canonicalize(&real_dir).unwrap();
        assert_eq!(state_dir(&alias), canonical_real_dir.join(".nile/nile"));
    }

    #[test]
    fn the_directory_config_round_trips_its_default_project() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_default_project(dir.path()).unwrap(), None);
        save_default_project(dir.path(), Some(Path::new("shows/trip.json"))).unwrap();
        assert_eq!(
            load_default_project(dir.path()).unwrap(),
            Some(PathBuf::from("shows/trip.json"))
        );
        save_default_project(dir.path(), None).unwrap();
        assert_eq!(load_default_project(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_corrupt_config_is_an_error_not_a_silent_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        std::fs::write(config_path(dir.path()), "{not json").unwrap();
        assert!(load_default_project(dir.path()).is_err());
    }
}
