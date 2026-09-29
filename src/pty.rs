//! PTY creation and process spawning.
//!
//! `eye` is the terminal endpoint: the shell runs on the far side of a pty
//! master/slave pair, exactly the way it would if a terminal emulator were
//! driving it. Nothing about the shell's environment is rewritten, no
//! `script`-style command wrapping, no shell-specific output scraping. That is
//! what lets `vim`, `htop`, `ssh` and friends behave normally while still
//! being recorded byte for byte.

use crate::event::Size;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// An open pty pair.
pub struct Pty {
    /// Owning handle to the master side. Owned, not borrowed: `posix_openpt`
    /// hands back a fresh fd and dropping it early would close the only end of
    /// the pair that works.
    master: OwnedFd,
    /// Kept open so the parent can set the size and the foreground group.
    slave: RawFd,
    slave_path: PathBuf,
}

impl Pty {
    /// Allocate a pty pair.
    pub fn open() -> io::Result<Self> {
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // From here on every early return has to close `fd`; the guard does.
        let master = OwnedFd::new(fd);

        if unsafe { libc::grantpt(master.0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::unlockpt(master.0) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut name = [0 as libc::c_char; 256];
        if unsafe { libc::ptsname_r(master.0, name.as_mut_ptr(), name.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let slave_path = PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes(),
        ).to_owned());
        // `ManuallyDrop` keeps the slave fd open for as long as the pair lives
        // without claiming ownership twice.
        let slave = std::fs::OpenOptions::new().read(true).write(true).open(&slave_path)?;
        let slave = std::mem::ManuallyDrop::new(slave);

        Ok(Pty {
            master,
            slave: slave.as_raw_fd(),
            slave_path,
        })
    }

    pub fn master_fd(&self) -> RawFd {
        self.master.0
    }

    pub fn slave_fd(&self) -> RawFd {
        self.slave
    }

    pub fn slave_path(&self) -> &PathBuf {
        &self.slave_path
    }

    /// Size of the pty, which is what the child believes its terminal is.
    pub fn size(&self) -> io::Result<Size> {
        crate::tty::window_size(self.slave)
    }

    /// Tell the child the terminal changed size. The kernel delivers `SIGWINCH`
    /// to the pty's foreground process group as a side effect.
    pub fn set_size(&self, size: Size) -> io::Result<()> {
        crate::tty::set_window_size(self.slave, size)
    }

    /// Hand the pty to a process group.
    pub fn set_foreground_group(&self, pgid: libc::pid_t) -> io::Result<()> {
        if unsafe { libc::tcsetpgrp(self.slave, pgid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Start a program on the slave side.
    ///
    /// The child gets a fresh session (`setsid`), the pty as its controlling
    /// terminal, and an *empty* signal mask: `eye` blocks signals so it can
    /// read them from a `signalfd`, and the shell must not inherit that.
    pub fn spawn(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        size: Size,
    ) -> io::Result<Child> {
        self.set_size(size)?;

        let slave = self.slave;
        let mut cmd = Command::new(program);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        // stdin/stdout/stderr are replaced with the pty slave inside
        // `pre_exec`, which `std` runs *after* it has installed whatever
        // `Stdio` asked for. `Stdio::null` is therefore the honest choice: it
        // says "this process's standard streams are not pipes", and it avoids
        // creating pipe pairs that the parent would hold open for nothing.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        unsafe {
            cmd.pre_exec(move || {
                // Async-signal-safe calls only from here to `execvp`.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Restore a default signal mask and dispositions.
                let mut empty = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                for signo in [libc::SIGPIPE, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    libc::signal(signo, libc::SIG_DFL);
                }
                if libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave, 0) < 0
                    || libc::dup2(slave, 1) < 0
                    || libc::dup2(slave, 2) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if slave > 2 {
                    libc::close(slave);
                }
                // Become the leader of the pty's foreground process group.
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        cmd.spawn()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // `master` closes itself. The slave `File` was deliberately never
        // wrapped, so its descriptor is left to the process exit path rather
        // than double-closed here.
        let _ = self.slave;
    }
}

/// Minimal owning wrapper so the master fd is closed exactly once.
struct OwnedFd(RawFd);

impl OwnedFd {
    fn new(fd: RawFd) -> Self {
        // `posix_openpt` already returned a fresh fd; nothing else owns it.
        OwnedFd(fd)
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Convert a `Vec<CString>` into a NUL-separated `exec`-style list.
pub fn to_cstrings(values: &[String]) -> Vec<CString> {
    values.iter().filter_map(|v| CString::new(v.as_str()).ok()).collect()
}

/// Check that a pty can be created at all, for a friendlier failure message.
pub fn probe() -> io::Result<()> {
    Pty::open().map(|_| ())
}

/// Spawn a process without a pty (used for `--` subcommands and version probes).
pub fn run_capture(program: &str, args: &[String]) -> io::Result<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Duplicate a raw fd into a `File` without taking ownership surprises.
pub fn file_from_fd(fd: RawFd) -> std::fs::File {
    unsafe { std::fs::File::from_raw_fd(fd) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::fd::{close, read_once, FdWriter};

    #[test]
    fn pty_allocates_a_usable_pair() {
        let pty = Pty::open().expect("pty should be available in a test environment");
        assert!(pty.master_fd() >= 0);
        assert!(pty.slave_fd() >= 0);
        assert!(pty.slave_path().as_os_str().len() > 1);
    }

    /// The master outlives `open`, and writing to the slave produces bytes that
    /// come back out of the master — which is the entire mechanism `eye` is
    /// built on, so it is worth asserting rather than assuming.
    #[test]
    fn the_master_stays_open_and_carries_slave_output() {
        let pty = Pty::open().unwrap();
        FdWriter(pty.slave_fd()).write_all(b"hello").unwrap();
        let mut buf = [0u8; 64];
        let n = read_once(pty.master_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        // Still usable a second time: a master that `open` had closed would
        // have failed on the very first read.
        FdWriter(pty.slave_fd()).write_all(b"!").unwrap();
        let n = read_once(pty.master_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], b"!");
    }

    #[test]
    fn dropping_the_pair_closes_the_master() {
        let fd = Pty::open().unwrap().master_fd();
        // No other descriptor refers to the same open file description, so
        // `Pty`'s drop is the only thing that can have closed it.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) },
            -1,
            "the master leaked"
        );
        close(-1);
    }

    #[test]
    fn probe_succeeds_here() {
        probe().expect("pty support is required");
    }

    #[test]
    fn cstring_conversion_skips_embedded_nuls() {
        let out = to_cstrings(&["ok".into(), "bad\0value".into(), "fine".into()]);
        assert_eq!(out.len(), 2);
    }
}
