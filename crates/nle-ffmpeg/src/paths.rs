//! Filesystem paths as ffmpeg/ffprobe arguments.
//!
//! `--` stops option parsing, but it does not stop the URL parser: a relative
//! `a:b.mp4` names protocol `a` and fails with "Protocol not found". A leading
//! `./` makes the path unambiguously local and leaves its location unchanged.

use std::path::{Component, Path};

/// Prefixes that are ffmpeg protocol escapes, not part of a local name:
/// `file:out.mp4` writes `out.mp4` and `pipe:1` is a stream. The user wrote
/// the protocol deliberately, so keep it.
const PROTOCOL_ESCAPES: [&str; 2] = ["file:", "pipe:"];

/// Render `path` so ffmpeg reads it as a file, not a URL protocol.
///
/// Only the *first* component matters — `/abs/a:b.mp4` and `sub/a:b.mp4`
/// already parse as paths and are returned unchanged. An explicit `file:` or
/// `pipe:` prefix is likewise left alone: nile output names are local unless
/// the user asks for a protocol.
pub fn local_path_arg(path: &Path) -> String {
    let protocol_like = matches!(
        path.components().next(),
        Some(Component::Normal(first))
            if {
                let first = first.to_string_lossy();
                first.contains(':') && !PROTOCOL_ESCAPES.iter().any(|p| first.starts_with(p))
            }
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

    #[test]
    fn an_explicit_protocol_escape_is_left_alone() {
        // `file:`/`pipe:` are the user's own escape: rewriting them would
        // turn `file:out.mp4` into a literal file named `file:out.mp4`.
        assert_eq!(local_path_arg(Path::new("file:out.mp4")), "file:out.mp4");
        assert_eq!(local_path_arg(Path::new("pipe:1")), "pipe:1");
        // Every other colon is still a local name.
        assert_eq!(
            local_path_arg(Path::new("files:out.mp4")),
            "./files:out.mp4"
        );
    }
}
