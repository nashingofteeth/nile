//! Tab completion for the `:` line's path arguments.
//!
//! `:import` is the fallback when yazi is absent, and typing a filename with
//! spaces there is painful. Completion is deliberately filesystem-only (no
//! shell): the last token is split into a directory and a prefix, the
//! directory is read, and the common prefix (or the single match) replaces
//! the token. A path with spaces comes back quoted.

pub(crate) struct Completion {
    pub buffer: String,
    pub note: Option<String>,
}

/// Complete the path token at the end of `buffer`. `None` means this line is
/// not a path command (or the token is already closed), so Tab stays inert.
pub(crate) fn complete_path(buffer: &str) -> Option<Completion> {
    let token_start = path_token(buffer)?;
    let token = &buffer[token_start..];
    let quote = match token.chars().next() {
        Some(q @ ('"' | '\'')) => Some(q),
        _ => None,
    };
    let body = match quote {
        Some(q) => {
            let rest = &token[q.len_utf8()..];
            // A closed quote is a finished argument; don't re-complete it.
            if rest.contains(q) {
                return None;
            }
            rest
        }
        None => token,
    };

    let (dir_part, prefix) = if body == "~" {
        ("~/", "")
    } else {
        match body.rfind('/') {
            Some(i) => (&body[..=i], &body[i + 1..]),
            None => ("", body),
        }
    };
    let fs_dir = expand_dir(dir_part);
    let Ok(entries) = std::fs::read_dir(&fs_dir) else {
        return Some(Completion {
            buffer: buffer.to_string(),
            note: Some(format!("no such directory: {dir_part}")),
        });
    };

    let mut candidates: Vec<(String, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        candidates.push((name, is_dir));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    if candidates.is_empty() {
        return Some(Completion {
            buffer: buffer.to_string(),
            note: Some("no match".to_string()),
        });
    }

    if candidates.len() == 1 {
        let (name, is_dir) = &candidates[0];
        let completed = format!("{dir_part}{name}{}", if *is_dir { "/" } else { "" });
        let replacement = quote_body(&completed, quote, !*is_dir);
        let mut new_buffer = buffer[..token_start].to_string();
        new_buffer.push_str(&replacement);
        if !*is_dir {
            new_buffer.push(' ');
        }
        return Some(Completion {
            buffer: new_buffer,
            note: None,
        });
    }

    let mut common = candidates[0].0.clone();
    for (name, _) in &candidates[1..] {
        common = common_prefix(&common, name);
    }
    if common.len() > prefix.len() {
        let completed = format!("{dir_part}{common}");
        let replacement = quote_body(&completed, quote, false);
        let mut new_buffer = buffer[..token_start].to_string();
        new_buffer.push_str(&replacement);
        return Some(Completion {
            buffer: new_buffer,
            note: Some(format!("{} matches", candidates.len())),
        });
    }

    let names: Vec<&str> = candidates
        .iter()
        .take(6)
        .map(|(name, _)| name.as_str())
        .collect();
    let more = candidates.len().saturating_sub(names.len());
    let suffix = if more > 0 {
        format!(" (+{more})")
    } else {
        String::new()
    };
    Some(Completion {
        buffer: buffer.to_string(),
        note: Some(format!(
            "{} matches: {}{suffix}",
            candidates.len(),
            names.join(" ")
        )),
    })
}

/// The byte offset where the path token starts, if this line accepts one.
fn path_token(buffer: &str) -> Option<usize> {
    let trimmed = buffer.trim_start();
    let leading = buffer.len() - trimmed.len();
    let verb = trimmed.split_whitespace().next()?;
    if verb != "import" && verb != "add" {
        return None;
    }
    let verb_end = leading + verb.len();
    if buffer.len() <= verb_end || !buffer[verb_end..].starts_with(char::is_whitespace) {
        return None;
    }
    let after = buffer[verb_end..].trim_start();
    if verb == "add" && (after.starts_with('-') || after.contains(" --")) {
        return None;
    }
    let start = buffer
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(verb_end);
    if verb == "add" && buffer[start..].starts_with('-') {
        return None;
    }
    Some(start)
}

fn expand_dir(dir_part: &str) -> std::path::PathBuf {
    if let Some(rest) = dir_part.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    if dir_part.is_empty() {
        std::path::PathBuf::from(".")
    } else {
        std::path::PathBuf::from(dir_part)
    }
}

fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

/// Quote a completed path when it contains whitespace or quote characters.
/// A directory leaves the quote open so the user can keep typing inside it.
fn quote_body(body: &str, preferred: Option<char>, close: bool) -> String {
    let quote = match preferred {
        Some('\'') if body.contains('\'') => Some('"'),
        Some(q) => Some(q),
        None if body
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '\\') =>
        {
            Some('"')
        }
        None => None,
    };
    let Some(q) = quote else {
        return body.to_string();
    };
    let mut escaped = String::new();
    if q == '"' {
        for c in body.chars() {
            if c == '\\' || c == '"' {
                escaped.push('\\');
            }
            escaped.push(c);
        }
    } else {
        escaped.push_str(body);
    }
    if close {
        format!("{q}{escaped}{q}")
    } else {
        format!("{q}{escaped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            if name.ends_with('/') {
                std::fs::create_dir(dir.path().join(name.trim_end_matches('/'))).unwrap();
            } else {
                std::fs::write(dir.path().join(name), b"").unwrap();
            }
        }
        dir
    }

    #[test]
    fn a_unique_file_completes_with_a_trailing_space() {
        let dir = dir_with(&["solo.mp4"]);
        let out = complete_path(&format!("import {}/so", dir.path().display())).unwrap();
        assert_eq!(
            out.buffer,
            format!("import {}/solo.mp4 ", dir.path().display())
        );
    }

    #[test]
    fn a_unique_directory_keeps_the_slash_and_the_prompt_open() {
        let dir = dir_with(&["footage/"]);
        let out = complete_path(&format!("import {}/fo", dir.path().display())).unwrap();
        assert_eq!(
            out.buffer,
            format!("import {}/footage/", dir.path().display())
        );
    }

    #[test]
    fn spaces_are_quoted_and_closed_for_a_file() {
        let dir = dir_with(&["my clip.mp4"]);
        let out = complete_path(&format!("import \"{}/my", dir.path().display())).unwrap();
        assert_eq!(
            out.buffer,
            format!("import \"{}/my clip.mp4\" ", dir.path().display())
        );
    }

    #[test]
    fn ambiguous_matches_extend_to_their_common_prefix() {
        let dir = dir_with(&["alpha.mp4", "alpine.mov"]);
        let out = complete_path(&format!("import {}/al", dir.path().display())).unwrap();
        assert_eq!(out.buffer, format!("import {}/alp", dir.path().display()));
        assert!(out.note.as_deref().unwrap_or("").contains("2 matches"));
    }

    #[test]
    fn a_second_tab_lists_the_matches() {
        let dir = dir_with(&["alpha.mp4", "alpine.mov"]);
        let out = complete_path(&format!("import {}/alp", dir.path().display())).unwrap();
        assert_eq!(out.buffer, format!("import {}/alp", dir.path().display()));
        let note = out.note.unwrap();
        assert!(note.contains("alpha.mp4"), "{note}");
        assert!(note.contains("alpine.mov"), "{note}");
    }

    #[test]
    fn add_only_completes_the_file_argument() {
        let dir = dir_with(&["solo.mp4"]);
        let out = complete_path(&format!("add {}/so", dir.path().display())).unwrap();
        assert!(out.buffer.ends_with("solo.mp4 "));
        assert!(complete_path("add file.mp4 --at 2").is_none());
        assert!(complete_path("export -o out.mp4").is_none());
    }

    #[test]
    fn a_closed_quote_is_not_recompleted() {
        assert!(complete_path("import \"done.mp4\"").is_none());
    }
}
