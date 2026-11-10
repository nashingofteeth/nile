//! The external import picker: yazi in chooser mode.
//!
//! The event loop restores the terminal before calling [`run_yazi`] and
//! re-initialises it afterwards. Yazi inherits the tty, so its own UI works;
//! the selection comes back through a temporary `--chooser-file` and the last
//! directory through `--cwd-file`. No shell, no fixed paths, and no state
//! file: the temp dir is dropped when this returns.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What yazi chose and where it was when it exited.
pub(crate) struct Picked {
    pub paths: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
}

/// Yazi is optional. `--version` is a cheap probe that does not need a tty.
pub(crate) fn yazi_available() -> bool {
    Command::new("yazi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run yazi as a multi-file chooser. `q`/an empty chooser file is a cancel,
/// not an error.
pub(crate) fn run_yazi(start: &Path) -> Result<Picked, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let chooser = dir.path().join("chooser");
    let cwd_file = dir.path().join("cwd");

    let status = Command::new("yazi")
        .arg("--chooser-file")
        .arg(&chooser)
        .arg("--cwd-file")
        .arg(&cwd_file)
        .arg("--")
        .arg(start)
        .status()
        .map_err(|e| format!("could not run yazi: {e}"))?;

    let cwd = read_cwd(&cwd_file).filter(|p| p.is_dir());
    let mut paths = read_paths(&chooser);
    // Yazi writes the selected URLs as given; a relative one is relative to
    // the directory it exited in.
    for path in &mut paths {
        if path.is_relative() {
            *path = match &cwd {
                Some(cwd) => cwd.join(&*path),
                None => start.join(&*path),
            };
        }
    }
    if !status.success() {
        return Err(format!("yazi exited with {status}"));
    }
    Ok(Picked { paths, cwd })
}

/// Read yazi's newline-delimited chooser file. Unlike fzf's NUL protocol this
/// cannot represent a filename containing a newline; the empty entries are
/// skipped so trailing newlines are harmless.
fn read_paths(path: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    bytes
        .split(|b| *b == b'\n')
        .map(|line| {
            if line.ends_with(b"\r") {
                &line[..line.len() - 1]
            } else {
                line
            }
        })
        .filter(|line| !line.is_empty())
        .map(path_from_bytes)
        .collect()
}

fn read_cwd(path: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(path).ok()?;
    let line = bytes.split(|b| *b == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        None
    } else {
        Some(path_from_bytes(line))
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn chooser_lines_become_paths() {
        let dir = tempfile::tempdir().unwrap();
        let chooser = dir.path().join("chooser");
        std::fs::write(&chooser, b"/a/one.mp4\n/b/two with space.mov\r\n\n").unwrap();
        assert_eq!(
            read_paths(&chooser),
            vec![
                PathBuf::from("/a/one.mp4"),
                PathBuf::from("/b/two with space.mov")
            ]
        );
    }

    #[test]
    fn a_missing_chooser_file_is_a_cancel() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_paths(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn the_cwd_file_is_one_path() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("cwd");
        std::fs::write(&cwd, b"/media/footage\n").unwrap();
        assert_eq!(read_cwd(&cwd), Some(PathBuf::from("/media/footage")));
    }

    #[test]
    fn non_utf8_names_survive_the_chooser_file() {
        let bytes = b"/media/\xffclip.mp4";
        let path = path_from_bytes(bytes);
        assert_eq!(path.as_os_str().as_bytes(), bytes);
    }
}
