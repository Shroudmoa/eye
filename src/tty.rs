//! Real-terminal control: raw mode, size, signals, key input.
//!
//! Everything here follows one rule: `eye` must never leave the user's
//! terminal in a state they did not ask for. Raw mode is restored from
//! `Drop`, from the signal path, and from an explicit `cleanup` call, and the
//! three of them all funnel into the same idempotent restore.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;

// ---------------------------------------------------------------------------
// Raw mode
// ---------------------------------------------------------------------------

/// Puts a terminal into the state `eye` needs to see every keystroke.
pub struct RawMode {
    fd: RawFd,
    original: Option<libc::termios>,
}

impl RawMode {
    /// Switch `fd` into raw mode.
    ///
    /// `OPOST` is deliberately left alone unless `exact_bytes` is set. During
    /// recording and replay `eye` wants the bytes it writes to the terminal to
    /// be exactly the bytes it recorded, with no `\n` -> `\r\n` rewriting
    /// happening behind its back; the PTY has already done that translation on
    /// the child's side.
    pub fn enable(fd: RawFd, exact_bytes: bool) -> io::Result<Self> {
        let original = unsafe { get_termios(fd)? };
        let mut raw = original;

        // Input: no line editing, no software flow control, no signal
        // generation. Ctrl-C must reach the child as byte 0x03 so the child's
        // own line discipline decides what it means.
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        // Local: no echo (the PTY's line discipline does that), no canonical
        // mode, no signal chars, no `^C` passthrough extensions.
        raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // Keep 8-bit clean regardless of what the user's terminal was doing.
        raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
        raw.c_cflag |= libc::CS8;
        if exact_bytes {
            raw.c_oflag &= !libc::OPOST;
        }

        set_termios(fd, &raw)?;
        Ok(RawMode { fd, original: Some(original) })
    }

    /// Restore the saved terminal settings. Idempotent.
    pub fn restore(&mut self) {
        if let Some(original) = self.original.take() {
            let _ = set_termios(self.fd, &original);
        }
    }

    pub fn is_active(&self) -> bool {
        self.original.is_some()
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

unsafe fn get_termios(fd: RawFd) -> io::Result<libc::termios> {
    let mut t = MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { t.assume_init() })
}

fn set_termios(fd: RawFd, t: &libc::termios) -> io::Result<()> {
    // TCSANOW: never block on output drain, which could deadlock a proxy.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Size
// ---------------------------------------------------------------------------

/// Ask the kernel how big the terminal is.
pub fn window_size(fd: RawFd) -> io::Result<crate::event::Size> {
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "terminal reports a zero size"));
    }
    Ok(crate::event::Size::new(ws.ws_col, ws.ws_row))
}

/// Push a new size into a pty.
pub fn set_window_size(fd: RawFd, size: crate::event::Size) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// True if `fd` refers to a terminal at all.
pub fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Signals `eye` intercepts. All are blocked process-wide so that they can be
/// consumed from a `signalfd` in the main `poll` loop instead of from an
/// async-signal handler.
pub const HANDLED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGWINCH,
    libc::SIGCHLD,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGPIPE,
];

/// Set of blocked signals as an OS-native `sigset_t`.
pub fn blocked_mask(extra: &[libc::c_int]) -> libc::sigset_t {
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::sigemptyset(&mut mask);
        for s in HANDLED_SIGNALS.iter().chain(extra) {
            libc::sigaddset(&mut mask, *s);
        }
    }
    mask
}

/// Install `mask` as the thread's signal mask.
pub fn block(mask: &libc::sigset_t) -> io::Result<()> {
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, mask, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Restore an all-open signal mask.
pub fn unblock_all() -> io::Result<()> {
    let mut empty = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe { libc::sigemptyset(&mut empty) };
    if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Temporarily install `mask`, run `f`, then put the previous mask back.
///
/// Used for job control: a stop signal has to be unblocked at the instant it
/// is raised or the kernel discards it.
pub fn with_mask<R>(mask: &libc::sigset_t, f: impl FnOnce() -> R) -> R {
    let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let swapped =
        unsafe { libc::sigprocmask(libc::SIG_SETMASK, mask, &mut previous) } == 0;
    let out = f();
    if swapped {
        unsafe { libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    }
    out
}

/// A readable fd that reports pending signals, like a socket but for signals.
pub struct SignalFd(std::fs::File);

impl SignalFd {
    pub fn new(mask: &libc::sigset_t) -> io::Result<Self> {
        let fd = unsafe {
            libc::signalfd(-1, mask as *const libc::sigset_t, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        use std::os::fd::FromRawFd;
        Ok(SignalFd(unsafe { std::fs::File::from_raw_fd(fd) }))
    }

    pub fn fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    /// Drain every pending signal. Signals that coalesce (standard signals
    /// pending twice become one) are reported once, which is all a proxy
    /// needs.
    pub fn drain(&self) -> Vec<libc::c_int> {
        let mut out = Vec::new();
        let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
        loop {
            let n = unsafe {
                libc::read(
                    self.fd(),
                    info.as_mut_ptr() as *mut libc::c_void,
                    std::mem::size_of::<libc::signalfd_siginfo>(),
                )
            };
            if n != std::mem::size_of::<libc::signalfd_siginfo>() as isize {
                break;
            }
            out.push(unsafe { info.assume_init() }.ssi_signo as libc::c_int);
        }
        out
    }
}

/// Human-readable name for a signal number.
pub fn signal_name(signo: libc::c_int) -> String {
    match unsafe { libc::strsignal(signo) } {
        p if p.is_null() => format!("SIG{signo}"),
        p => unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned(),
    }
}

/// Short canonical name (`INT`, `WINCH`, ...) used in event records.
pub fn short_signal_name(signo: libc::c_int) -> String {
    const NAMES: &[(libc::c_int, &str)] = &[
        (libc::SIGHUP, "HUP"),
        (libc::SIGINT, "INT"),
        (libc::SIGQUIT, "QUIT"),
        (libc::SIGILL, "ILL"),
        (libc::SIGABRT, "ABRT"),
        (libc::SIGFPE, "FPE"),
        (libc::SIGKILL, "KILL"),
        (libc::SIGSEGV, "SEGV"),
        (libc::SIGPIPE, "PIPE"),
        (libc::SIGALRM, "ALRM"),
        (libc::SIGTERM, "TERM"),
        (libc::SIGUSR1, "USR1"),
        (libc::SIGUSR2, "USR2"),
        (libc::SIGCHLD, "CHLD"),
        (libc::SIGCONT, "CONT"),
        (libc::SIGTSTP, "TSTP"),
        (libc::SIGTTIN, "TTIN"),
        (libc::SIGTTOU, "TTOU"),
        (libc::SIGWINCH, "WINCH"),
    ];
    match NAMES.iter().find(|(n, _)| *n == signo) {
        Some((_, name)) => (*name).to_owned(),
        None => format!("SIG{signo}"),
    }
}

/// Send a signal to a process group.
pub fn signal_group(pgid: libc::pid_t, signo: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(-pgid, signo) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_cover_the_signals_we_forward() {
        assert_eq!(short_signal_name(libc::SIGINT), "INT");
        assert_eq!(short_signal_name(libc::SIGWINCH), "WINCH");
        assert_eq!(short_signal_name(libc::SIGTSTP), "TSTP");
        assert_eq!(short_signal_name(libc::SIGCHLD), "CHLD");
        assert_eq!(short_signal_name(9999), "SIG9999");
        assert!(!signal_name(libc::SIGINT).is_empty());
    }

    #[test]
    fn masks_contain_every_handled_signal() {
        let mask = blocked_mask(&[]);
        for s in HANDLED_SIGNALS {
            assert_eq!(unsafe { libc::sigismember(&mask, *s) }, 1, "missing {}", short_signal_name(*s));
        }
        let with_extra = blocked_mask(&[libc::SIGUSR1]);
        assert_eq!(unsafe { libc::sigismember(&with_extra, libc::SIGUSR1) }, 1);
    }

    #[test]
    fn size_rejects_a_non_terminal() {
        // fd 0 may be a terminal under `cargo test`; either way this must not
        // panic and must not report a bogus non-zero size.
        if let Ok(size) = window_size(0) {
            assert!(size.cols > 0 && size.rows > 0);
        }
    }
}
