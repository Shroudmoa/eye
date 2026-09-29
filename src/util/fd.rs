//! Thin, allocation-free wrappers over raw file descriptors.
//!
//! The proxy loop reads and writes fds thousands of times a second, so it skips
//! `std::io` entirely: no locking, no buffering, no ownership ambiguity about
//! fds 0/1/2.

use std::io;
use std::os::fd::RawFd;

/// Default buffer size for a single read. Big enough that a full-screen redraw
/// usually arrives in one syscall.
pub const BUF: usize = 64 * 1024;

/// A borrowed fd that can be written to without taking ownership of it.
#[derive(Clone, Copy, Debug)]
pub struct FdWriter(pub RawFd);

impl io::Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_once(self.0, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FdWriter {
    /// Write everything, looping over short writes and `EINTR`.
    pub fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        write_all_loop(|chunk| write_once(self.0, chunk), buf)
    }
}

/// The short-write loop, factored out so it can be tested without a pipe.
///
/// A blocking `write(2)` to a full pipe holds the thread — and, in a threaded
/// program, the global lock that keeps the reader on the same thread from
/// running — so a threaded pipe test would deadlock rather than demonstrate
/// anything. Driving the loop with an injected writer tests the same logic
/// without that trap.
fn write_all_loop(
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
    buf: &[u8],
) -> io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        match write(&buf[written..]) {
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned zero"))
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One `write(2)`, retrying `EINTR`.
fn write_once(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Read once into `buf`, retrying `EINTR`.
///
/// Returns `Ok(0)` at end of file. A pty master reports the slave closing as
/// `EIO` on Linux, which callers should treat as end of file; that translation
/// is left to them because only the pty loop knows it is a pty.
pub fn read_once(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// True if the error means "there is nothing left to read here".
pub fn is_eof_like(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EIO) | Some(libc::EPIPE))
        || err.kind() == io::ErrorKind::UnexpectedEof
}

/// Close an fd, ignoring errors. Used on paths where the fd is going away
/// regardless.
pub fn close(fd: RawFd) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// Set or clear `O_NONBLOCK`.
pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = if on { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-blocking mode as a scoped change, undone on drop.
///
/// `O_NONBLOCK` lives on the *open file description*, not on the descriptor, so
/// setting it on a descriptor that shares its file description — which stdin
/// and stdout do whenever both point at the terminal — silently makes the
/// terminal's writes non-blocking too. Callers that do this on a terminal must
/// therefore not: for a tty, raw mode's `VMIN`/`VTIME` already guarantee that a
/// `read` after `poll` reports readable has bytes waiting. This exists for the
/// pipes and files where blocking would mean an unbounded `poll` timeout.
pub struct NonBlocking {
    fd: RawFd,
    restore: bool,
}

impl NonBlocking {
    /// Put `fd` into non-blocking mode, remembering whether it already was.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let restore = flags & libc::O_NONBLOCK == 0;
        if restore {
            set_nonblocking(fd, true)?;
        }
        Ok(NonBlocking { fd, restore })
    }
}

impl Drop for NonBlocking {
    fn drop(&mut self) {
        if self.restore {
            let _ = set_nonblocking(self.fd, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_reach_a_pipe() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let writer = FdWriter(w);
        writer.write_all(b"hello ").unwrap();
        writer.write_all(b"world").unwrap();
        let mut buf = [0u8; 16];
        let n = read_once(r, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello world");
        close(r);
        close(w);
    }

    #[test]
    fn large_writes_survive_short_write_loops() {
        // A pipe accepts at most 64 kB at a time, so anything larger than the
        // buffer is necessarily delivered in pieces. The injected writer below
        // is the pathological version of that: seven bytes at a time.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut seen = Vec::new();
        let mut calls = 0usize;
        let mut offset = 0usize;
        write_all_loop(
            |chunk| {
                calls += 1;
                let n = chunk.len().min(7);
                seen.extend_from_slice(&chunk[..n]);
                offset += n;
                Ok(n)
            },
            &payload,
        )
        .unwrap();
        assert_eq!(offset, payload.len());
        assert!(calls > 1, "the payload should not have fitted in one call");
        assert_eq!(seen, payload);
    }

    #[test]
    fn a_writer_that_returns_zero_is_an_error_not_an_infinite_loop() {
        let err = write_all_loop(|_| Ok(0), b"hello").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    }

    #[test]
    fn interruptions_are_retried_and_real_errors_are_returned() {
        let mut first = true;
        let mut calls = 0;
        write_all_loop(
            |chunk| {
                calls += 1;
                if first {
                    first = false;
                    return Err(io::Error::from_raw_os_error(libc::EINTR));
                }
                Ok(chunk.len())
            },
            b"hello",
        )
        .unwrap();
        assert_eq!(calls, 2);

        let err = write_all_loop(|_| Err(io::Error::from_raw_os_error(libc::EBADF)), b"x")
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn eof_like_errors_are_recognised() {
        assert!(is_eof_like(&io::Error::from_raw_os_error(libc::EIO)));
        assert!(is_eof_like(&io::Error::from_raw_os_error(libc::EPIPE)));
        assert!(!is_eof_like(&io::Error::from_raw_os_error(libc::EBADF)));
    }

    #[test]
    fn non_blocking_mode_is_restored_on_drop() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let is_nonblocking = |fd| {
            (unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK) != 0
        };
        assert!(!is_nonblocking(r));
        {
            let _guard = NonBlocking::new(r).unwrap();
            assert!(is_nonblocking(r), "the guard must apply the change immediately");
            // With nothing to read and no timeout, the read must come back
            // rather than park the thread.
            let mut buf = [0u8; 8];
            assert_eq!(
                read_once(r, &mut buf).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        assert!(!is_nonblocking(r), "the flag must not outlive the guard");
        close(r);
        close(w);
    }

    #[test]
    fn an_already_non_blocking_fd_is_left_alone() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        set_nonblocking(r, true).unwrap();
        {
            let _guard = NonBlocking::new(r).unwrap();
        }
        assert!(
            (unsafe { libc::fcntl(r, libc::F_GETFL) } & libc::O_NONBLOCK) != 0,
            "restoring must not undo a change the caller made itself"
        );
        close(r);
        close(w);
    }

    #[test]
    fn a_bad_fd_is_reported_rather_than_silently_accepted() {
        assert!(NonBlocking::new(-1).is_err());
    }
}
