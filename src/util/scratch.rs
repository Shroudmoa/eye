//! A private temporary directory, removed when it goes out of scope.
//!
//! Shell integration needs somewhere to put a generated rcfile. It is the one
//! thing `eye` writes to disk outside of the recording itself, so it is kept
//! deliberately minimal: a predictable name under `$TMPDIR`, `0700`, and an
//! unconditional `remove_dir_all` on drop. A full-blown temporary-file crate
//! would buy crash-cleanup guarantees that a stray `eye-*-<pid>` directory
//! cannot really offer anyway.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that exists only for as long as this value does.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create `eye-<tag>-<pid>-<n>` under the system temporary directory.
    pub fn new(tag: &str) -> io::Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("eye-{tag}-{}-{n}", std::process::id()));
        // A leftover directory from a crashed process must not be reused.
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir_all(&path)?;
        set_private(&path)?;
        Ok(Scratch { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Best-effort `0700`. A generated rcfile can contain nothing secret today, but
/// a world-readable directory in a shared `/tmp` is a habit worth not having.
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_created_and_removed() {
        let path = {
            let dir = Scratch::new("test").unwrap();
            assert!(dir.path().is_dir());
            std::fs::write(dir.path().join("f"), b"x").unwrap();
            dir.path().to_path_buf()
        };
        assert!(!path.exists(), "drop must remove the directory");
    }

    #[test]
    fn scratch_directories_do_not_collide() {
        let a = Scratch::new("test").unwrap();
        let b = Scratch::new("test").unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn a_stale_directory_from_a_crashed_run_is_replaced() {
        let path = std::env::temp_dir().join(format!("eye-crash-{}-0", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("stale"), b"x").unwrap();
        {
            let dir = Scratch::new("crash").unwrap();
            assert!(!dir.path().join("stale").exists());
        }
    }
}
