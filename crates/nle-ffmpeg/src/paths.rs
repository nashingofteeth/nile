//! Filesystem paths as ffmpeg/ffprobe arguments.
//!
//! `--` stops option parsing, but it does not stop the URL parser: a relative
//! `a:b.mp4` names protocol `a` and fails with "Protocol not found". A leading
//! `./` makes the path unambiguously local and leaves its location unchanged.

use std::path::{Component, Path};

/// Render `path` so ffmpeg reads it as a file, not a URL protocol.
///
/// Only the *first* component matters — `/abs/a:b.mp4` and `sub/a:b.mp4`
/// already parse as paths and are returned unchanged.
pub fn local_path_arg(path: &Path) -> String {
    let protocol_like = matches!(
        path.components().next(),
        Some(Component::Normal(first)) if first.to_string_lossy().contains(':')
    );
    if protocol_like {
        format!("./{}", path.display())
    } else {
        path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protocol_like_path_is_rendered_as_a_local_file() {
        // `--` guards option injection, not ffmpeg's URL parser: a first
        // component with `:` needs `./` to stay a file. Nested and absolute
        // paths already parse as paths and must not move.
        assert_eq!(local_path_arg(Path::new("out.mp4")), "out.mp4");
        assert_eq!(local_path_arg(Path::new("a:b.mp4")), "./a:b.mp4");
        assert_eq!(local_path_arg(Path::new("sub/a:b.mp4")), "sub/a:b.mp4");
        assert_eq!(local_path_arg(Path::new("/abs/a:b.mp4")), "/abs/a:b.mp4");
        assert_eq!(local_path_arg(Path::new("./a:b.mp4")), "./a:b.mp4");
        assert_eq!(local_path_arg(Path::new("../a:b.mp4")), "../a:b.mp4");
    }
}
