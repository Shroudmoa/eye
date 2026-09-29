//! Where recordings live, and how to find them.

pub mod reader;
pub mod writer;

pub use reader::{read_all_tolerant, ReadError, SessionReader, SessionSummary};
pub use writer::{is_part, part_path_for, SessionWriter};

use crate::util::time;
use std::path::{Path, PathBuf};

/// Extension used by finished recordings.
pub const EXT: &str = "eye";

/// Directory recordings are written to.
///
/// `$EYE_DIR` wins, then `$XDG_DATA_HOME/eye`, then `$HOME/.local/share/eye`.
pub fn sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("EYE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("eye");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/eye");
    }
    PathBuf::from(".eye")
}

/// The default filename for a session starting now: `session-2026-09-29.eye`.
pub fn default_filename() -> String {
    format!("session-{}.{EXT}", time::format_date(time::now_unix()))
}

/// A non-colliding path in `dir`, appending `.2`, `.3`, ... if needed.
///
/// Overwriting a recording is almost never what the user meant, and silently
/// destroying one is the one unrecoverable mistake `eye` could make.
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let stem = filename.strip_suffix(&format!(".{EXT}")).unwrap_or(filename);
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem}.{n}.{EXT}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

/// Default destination for a new session, uniquified.
pub fn default_session_path() -> PathBuf {
    let dir = sessions_dir();
    unique_path(&dir, &default_filename())
}

/// Every recording in `dir`, finished ones first, then newest.
pub fn list_sessions(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut finished: Vec<PathBuf> = Vec::new();
    let mut partial: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != EXT) {
            continue;
        }
        if is_part(&path) {
            partial.push(path);
        } else {
            finished.push(path);
        }
    }
    // Names are date-prefixed and uniquified, so lexical order is chronological.
    finished.sort();
    finished.reverse();
    partial.sort();
    partial.reverse();
    finished.extend(partial);
    finished
}

/// Resolve a user-supplied session argument.
///
/// Bare names are looked up in the sessions directory, so `eye replay
/// session.eye` works from anywhere; anything containing a separator is treated
/// as a path.
pub fn resolve_session(arg: &str) -> PathBuf {
    let as_path = Path::new(arg);
    if as_path.components().count() > 1 || arg.contains('/') {
        return as_path.to_path_buf();
    }
    let direct = sessions_dir().join(arg);
    if direct.exists() {
        return direct;
    }
    as_path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_only_disambiguates_when_needed() {
        let dir = std::env::temp_dir().join(format!("eye-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = "session-2026-09-29.eye";
        assert_eq!(unique_path(&dir, name), dir.join(name));
        std::fs::write(dir.join(name), b"x").unwrap();
        assert_eq!(unique_path(&dir, name), dir.join(format!("session-2026-09-29.2.eye")));
        std::fs::write(dir.join("session-2026-09-29.2.eye"), b"x").unwrap();
        assert_eq!(unique_path(&dir, name), dir.join(format!("session-2026-09-29.3.eye")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_names_resolve_against_the_sessions_dir() {
        let dir = std::env::temp_dir().join(format!("eye-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.eye"), b"x").unwrap();
        // SAFETY: single-threaded test process section below.
        unsafe { std::env::set_var("EYE_DIR", &dir) };
        assert_eq!(resolve_session("s.eye"), dir.join("s.eye"));
        assert_eq!(resolve_session("/tmp/abs.eye"), PathBuf::from("/tmp/abs.eye"));
        unsafe { std::env::remove_var("EYE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
