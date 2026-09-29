//! Streaming writer for `.eye` files.
//!
//! Durability rules:
//!
//! * The header is written and flushed before the shell starts, so even a
//!   session killed immediately leaves a file we can name and inspect.
//! * The live recording is written to `<name>.eye.part`. It becomes
//!   `<name>.eye` only via [`SessionWriter::finish`], after the trailer is
//!   written and the file is `fsync`ed. A crashed session therefore leaves a
//!   recoverable `.part` and never a half-written `.eye`.
//! * Readers are told about `.part` files rather than being left to guess.

use crate::event::{Event, Meta};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Suffix used for an in-progress recording.
pub const PART_SUFFIX: &str = "part";

struct Inner {
    out: BufWriter<File>,
    events: u64,
}

/// An open, appendable session file.
pub struct SessionWriter {
    inner: Option<Inner>,
    final_path: PathBuf,
    part_path: PathBuf,
    meta: Meta,
}

impl SessionWriter {
    /// Start a recording, writing the header immediately.
    pub fn create(path: &Path, meta: Meta) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let part_path = part_path_for(path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part_path)?;
        let mut out = BufWriter::with_capacity(64 * 1024, file);
        write_line(&mut out, &Event::Meta { meta: Box::new(meta.clone()) })?;
        out.flush()?;

        Ok(SessionWriter {
            inner: Some(Inner { out, events: 1 }),
            final_path: path.to_path_buf(),
            part_path,
            meta,
        })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Path the recording will occupy once finished.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Where bytes are going right now.
    pub fn part_path(&self) -> &Path {
        &self.part_path
    }

    /// Append an event. A serialisation failure leaves the writer usable so
    /// that one bad event cannot end a long session early.
    pub fn push(&mut self, event: &Event) -> io::Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(io::Error::other("writer already closed"));
        };
        write_line(&mut inner.out, event)?;
        inner.events += 1;
        Ok(())
    }

    /// Number of records written so far, including the header.
    pub fn events(&self) -> u64 {
        self.inner.as_ref().map_or(0, |i| i.events)
    }

    /// Push everything to the OS without finalising the file.
    pub fn flush(&mut self) -> io::Result<()> {
        match self.inner.as_mut() {
            Some(inner) => inner.out.flush(),
            None => Ok(()),
        }
    }

    /// Write the trailer, sync, and atomically move the file into place.
    ///
    /// Takes `&mut self` rather than `self` so that a caller holding the
    /// writer can finalise it mid-structure; the writer is left closed and
    /// every later call is a no-op or an error, never a second commit.
    pub fn finish(&mut self, reason: &str, code: Option<i32>) -> io::Result<PathBuf> {
        let mut inner = self
            .inner
            .take()
            .ok_or_else(|| io::Error::other("recording already finished"))?;
        let trailer = Event::SessionEnd { t: 0.0, reason: reason.to_owned(), code };
        write_line(&mut inner.out, &trailer)?;
        inner.out.flush()?;
        inner.out.get_ref().sync_all()?;
        drop(inner.out);
        fs::rename(&self.part_path, &self.final_path)?;
        Ok(self.final_path.clone())
    }

    /// Give up on finalising. The `.part` file is left in place and remains
    /// readable, which is what makes an interrupted session recoverable.
    pub fn abandon(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let _ = inner.out.flush();
        }
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let _ = inner.out.flush();
        }
    }
}

fn write_line<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *w, value).map_err(io::Error::other)?;
    w.write_all(b"\n")
}

/// `<name>.eye.part` for a recording destined for `<name>.eye`.
pub fn part_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(PART_SUFFIX);
    path.with_file_name(name)
}

/// True if `path` names an unfinished recording.
pub fn is_part(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == PART_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::meta;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eye-w-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_path(name: &str) -> PathBuf {
        temp_dir(name).join("session.eye")
    }

    #[test]
    fn header_is_written_before_any_events() {
        let path = temp_path("header");
        let mut w = SessionWriter::create(&path, meta()).unwrap();
        let text = fs::read_to_string(w.part_path()).unwrap();
        assert!(text.starts_with("{\"type\":\"meta\""));
        assert!(!path.exists(), "final path must not appear until finish");
        w.abandon();
    }

    #[test]
    fn finish_makes_the_file_visible_and_writes_a_trailer() {
        let path = temp_path("finish");
        let mut w = SessionWriter::create(&path, meta()).unwrap();
        w.push(&Event::Mark { t: 1.0, label: "x".into(), note: None }).unwrap();
        assert_eq!(w.events(), 2);
        let out = w.finish("shell_exit", Some(0)).unwrap();
        assert_eq!(out, path);
        assert!(!part_path_for(&path).exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("\"type\":\"session_end\",\"t\":0.0,\"reason\":\"shell_exit\",\"code\":0}\n"));
    }

    #[test]
    fn abandoning_leaves_a_recoverable_part_file() {
        let path = temp_path("abandon");
        let mut w = SessionWriter::create(&path, meta()).unwrap();
        w.push(&Event::Mark { t: 1.0, label: "x".into(), note: None }).unwrap();
        let part = w.part_path().to_path_buf();
        w.abandon();
        assert!(part.exists());
        assert!(!path.exists());
        assert!(is_part(&part));
    }

    #[test]
    fn one_event_per_line() {
        let path = temp_path("lines");
        let mut w = SessionWriter::create(&path, meta()).unwrap();
        for i in 0..5 {
            w.push(&Event::Mark { t: i as f64, label: "m".into(), note: None }).unwrap();
        }
        w.finish("eof", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 7);
    }

    #[test]
    fn nested_directories_are_created() {
        let path = temp_dir("nested").join("a/b/c.eye");
        let mut w = SessionWriter::create(&path, meta()).unwrap();
        assert!(w.part_path().exists());
        w.abandon();
    }
}
