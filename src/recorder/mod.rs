//! Recording: a byte-faithful proxy between the user's terminal and a shell.
//!
//! The loop is the classic `poll` proxy — read from the pty master, read from
//! the user's terminal, read from a `signalfd` — with three properties that
//! matter more than throughput:
//!
//! * **What is drawn is what is recorded.** Every chunk read from the master is
//!   written to the terminal and stored as one `Output` event, in that order,
//!   with no normalisation in between. Replay is then a byte-exact re-emission
//!   rather than an interpretation.
//! * **Input is recorded before it is forwarded.** The keystroke stream is
//!   stored first so that a crash between the read and the write still leaves
//!   the record complete.
//! * **The user's terminal is never left broken.** Raw mode is owned by a
//!   [`RawMode`] guard that restores it from `Drop`, from the job-control stop
//!   path, and on the way out of every error. Signal handling uses a blocked
//!   mask plus `signalfd`, so there are no async-signal handlers that could
//!   deadlock against a `write` in progress.
//!
//! No async runtime is used: there is exactly one task, it is blocked on three
//! file descriptors, and an executor would add a dependency without removing
//! any work.

pub mod commands;
pub mod shell;

use crate::event::{
    Bytes, Event, Meta, ShellInfo, Size, Stream, TerminalInfo, FORMAT_NAME, FORMAT_VERSION,
};
use crate::pty::Pty;
use crate::storage::SessionWriter;
use crate::tty::{self, RawMode, SignalFd};
use crate::util::{fd, scratch::Scratch, time};
use anyhow::{anyhow, bail, Context, Result};
use commands::CommandTracker;
use shell::{Integration, Shell};
use std::os::fd::RawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::Instant;

/// How long to keep draining the pty after the child is gone before deciding
/// that a surviving grandchild is holding the terminal open forever.
const DRAIN_POLL_MS: i32 = 200;
const DRAIN_ROUNDS: u32 = 3;

/// What the user asked for.
#[derive(Debug, Clone, Default)]
pub struct RecordOptions {
    /// Where to write the recording. `None` picks a dated, uniquified name in
    /// the sessions directory.
    pub path: Option<PathBuf>,
    /// A shell to run instead of `$SHELL`.
    pub shell: Option<Shell>,
    /// Working directory for the shell.
    pub cwd: Option<PathBuf>,
    /// Free-form label stored in the header.
    pub title: Option<String>,
    /// Store keystrokes as `Input` events. Off by default: replay does not
    /// need them, and they double the size of a session.
    pub record_input: bool,
    /// Skip shell integration, using echo reconstruction only.
    pub no_integration: bool,
    /// Overrides for tests: where to read keystrokes and where to draw.
    pub input_fd: Option<RawFd>,
    pub output_fd: Option<RawFd>,
}

impl RecordOptions {
    pub fn input_fd(&self) -> RawFd {
        self.input_fd.unwrap_or(0)
    }
    pub fn output_fd(&self) -> RawFd {
        self.output_fd.unwrap_or(1)
    }
}

/// What a finished session produced.
#[derive(Debug, Clone)]
pub struct RecordOutcome {
    /// Final path, after the `.part` file was committed.
    pub path: PathBuf,
    pub duration: f64,
    /// Exit status of the shell, if it exited normally.
    pub exit_code: Option<i32>,
    /// The `session_end` reason: `shell_exit`, `signal` or `eof`.
    pub reason: &'static str,
    pub events: u64,
}

/// Record a session. Blocks until the shell exits.
///
/// On any error the partially written `.part` file is left in place and stays
/// readable, because a recording that stopped at 90% is worth more than no
/// recording at all.
pub fn record(opts: &RecordOptions) -> Result<RecordOutcome> {
    let input = opts.input_fd();
    let output = opts.output_fd();
    if !tty::is_tty(input) {
        bail!("eye needs a terminal on stdin to record from; run it from a terminal, not a pipe");
    }
    if !tty::is_tty(output) {
        bail!("eye needs a terminal on stdout to draw the session on; redirect the recording with `-o FILE`");
    }

    // We deliberately move the *pty's* foreground process group around, and
    // the kernel punishes a background process that does so with SIGTTOU. The
    // user's real terminal keeps its own foreground group untouched, so this
    // is invisible from outside.
    ignore_job_control_signals();

    let mask = tty::blocked_mask(&[]);
    tty::block(&mask).context("block signals for signalfd")?;
    let signals = SignalFd::new(&mask).context("create a signalfd")?;

    let raw = RawMode::enable(input, true).context("switch the terminal to raw mode")?;
    let pty = Pty::open().context("open a pseudo-terminal")?;
    let size = tty::window_size(input).or_else(|_| pty.size()).unwrap_or_else(|_| Size::fallback());

    let shell = match &opts.shell {
        Some(shell) => shell.clone(),
        None => shell::detect().map_err(|e| anyhow!(e))?,
    };
    let shell = if opts.no_integration {
        Shell { integration: Integration::Echo, ..shell }
    } else {
        shell
    };

    let scratch = Scratch::new("session").context("create a scratch directory")?;
    let inv = shell.invocation(scratch.path());
    let env = shell.env(&inv.env);
    let meta = build_meta(&inv, size, opts);
    let path = opts.path.clone().unwrap_or_else(crate::storage::default_session_path);

    let mut writer = SessionWriter::create(&path, meta)
        .with_context(|| format!("create a recording at {}", path.display()))?;
    writer.push(&Event::SessionStart { t: 0.0, size })?;

    let child = match pty.spawn(&inv.program, &inv.args, &env, opts.cwd.as_deref(), size) {
        Ok(child) => child,
        Err(e) => {
            // The `.part` file already holds a valid header; leave it for the
            // user rather than pretending the session never happened.
            let part = writer.part_path().to_path_buf();
            drop(writer);
            return Err(e).with_context(|| {
                format!("start {}; a partial recording was kept at {}", inv.program, part.display())
            });
        }
    };

    let mut session = Session {
        pty,
        raw,
        writer,
        signals,
        out: fd::FdWriter(output),
        input,
        child,
        pgid: 0,
        start: Instant::now(),
        opts,
        tracker: CommandTracker::new(inv.integration == Integration::Osc133),
        events: Vec::new(),
        status: None,
        master_open: true,
        stdin_open: true,
        drain_rounds: 0,
    };
    session.pgid = session.child.id() as libc::pid_t;
    // Hand the pty to the child so that Ctrl-Z, Ctrl-\ and job control inside
    // the session reach the shell rather than `eye`.
    let _ = session.pty.set_foreground_group(session.pgid);

    let result = session.run();
    // Whatever happened, the user's terminal goes back the way it was.
    session.raw.restore();
    result
}

/// The live state of one recording.
struct Session<'a> {
    pty: Pty,
    raw: RawMode,
    writer: SessionWriter,
    signals: SignalFd,
    out: fd::FdWriter,
    input: RawFd,
    child: Child,
    pgid: libc::pid_t,
    start: Instant,
    opts: &'a RecordOptions,
    tracker: CommandTracker,
    /// Reused so that the hot loop does not allocate per chunk.
    events: Vec<Event>,
    status: Option<ExitStatus>,
    master_open: bool,
    stdin_open: bool,
    drain_rounds: u32,
}

const F_SIGNAL: usize = 0;
const F_STDIN: usize = 1;
const F_MASTER: usize = 2;
const NFDS: libc::nfds_t = 3;

impl Session<'_> {
    fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn run(&mut self) -> Result<RecordOutcome> {
        let mut fds = [
            libc::pollfd { fd: self.signals.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.input, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.pty.master_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // One buffer for the whole session. It lives here rather than on
        // `self` so that handing a slice of it to `&mut self` methods is not a
        // self-borrow.
        let mut buf = vec![0u8; fd::BUF];

        while self.master_open {
            fds[F_SIGNAL].fd = self.signals.fd();
            fds[F_SIGNAL].events = libc::POLLIN;
            fds[F_SIGNAL].revents = 0;
            fds[F_STDIN].fd = if self.stdin_open { self.input } else { -1 };
            fds[F_STDIN].events = libc::POLLIN;
            fds[F_STDIN].revents = 0;
            fds[F_MASTER].fd = if self.master_open { self.pty.master_fd() } else { -1 };
            fds[F_MASTER].events = libc::POLLIN;
            fds[F_MASTER].revents = 0;

            // Once the shell is gone we still want to drain the tail of its
            // output, but not forever: a background daemon holding the pty
            // open would otherwise hang the session.
            let timeout = if self.status.is_some() { DRAIN_POLL_MS } else { -1 };

            let n = unsafe { libc::poll(fds.as_mut_ptr(), NFDS, timeout) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("poll");
            }
            if n == 0 {
                self.drain_rounds += 1;
                if self.drain_rounds >= DRAIN_ROUNDS {
                    self.master_open = false;
                }
                continue;
            }
            self.drain_rounds = 0;

            // Signals first: a reaped child unblocks further pty reads, and
            // input before output keeps a keystroke ahead of the echo it
            // causes rather than behind it.
            if fds[F_SIGNAL].revents != 0 {
                self.handle_signals()?;
            }
            if fds[F_STDIN].revents != 0 {
                self.pump_stdin(&mut buf)?;
            }
            if fds[F_MASTER].revents != 0 {
                self.pump_master(&mut buf)?;
            }
        }

        self.finish()
    }

    /// Drain everything the shell has written so far.
    fn pump_master(&mut self, buf: &mut [u8]) -> Result<()> {
        while self.master_open {
            match fd::read_once(self.pty.master_fd(), buf) {
                Ok(0) => {
                    self.master_open = false;
                    return Ok(());
                }
                Ok(n) => {
                    let (chunk, _) = buf.split_at(n);
                    self.on_output(chunk)?;
                    // A short read means the pty queue is empty; `poll` will
                    // wake us again if more arrives.
                    if n < buf.len() {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // A pty reports the last slave closing as EIO.
                Err(e) if fd::is_eof_like(&e) => {
                    self.master_open = false;
                    return Ok(());
                }
                Err(e) => return Err(e).context("read from the pty"),
            }
        }
        Ok(())
    }

    /// Drain everything the user has typed so far.
    fn pump_stdin(&mut self, buf: &mut [u8]) -> Result<()> {
        while self.stdin_open {
            match fd::read_once(self.input, buf) {
                Ok(0) => {
                    // The terminal went away. Stop reading, but keep relaying
                    // whatever the shell still says.
                    self.stdin_open = false;
                    return Ok(());
                }
                Ok(n) => {
                    let (chunk, _) = buf.split_at(n);
                    self.on_input(chunk)?;
                    if n < buf.len() {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if fd::is_eof_like(&e) => {
                    self.stdin_open = false;
                    return Ok(());
                }
                Err(e) => return Err(e).context("read from the terminal"),
            }
        }
        Ok(())
    }

    /// Record bytes the shell produced, then draw them.
    fn on_output(&mut self, bytes: &[u8]) -> Result<()> {
        let t = self.elapsed();
        // Store first: a failure to draw should not cost us the record.
        self.writer
            .push(&Event::Output { t, data: Bytes::from(bytes), stream: Stream::Tty })?;
        self.out.write_all(bytes).context("write to the terminal")?;

        self.events.clear();
        self.tracker.on_output(t, bytes, &mut self.events);
        for event in self.events.drain(..) {
            self.writer.push(&event)?;
        }
        Ok(())
    }

    /// Record bytes the user typed, then forward them.
    fn on_input(&mut self, bytes: &[u8]) -> Result<()> {
        let t = self.elapsed();
        if self.opts.record_input {
            self.writer.push(&commands::input_event(t, bytes))?;
        }

        match fd::FdWriter(self.pty.master_fd()).write_all(bytes) {
            Ok(()) => {}
            // The shell is gone; the pty is closed for writing.
            Err(e) if fd::is_eof_like(&e) => {
                self.stdin_open = false;
                return Ok(());
            }
            Err(e) => return Err(e).context("write to the pty"),
        }

        self.events.clear();
        self.tracker.on_input(t, bytes, &mut self.events);
        for event in self.events.drain(..) {
            self.writer.push(&event)?;
        }

        // Ctrl-Z reaches the pty's line discipline, which stops the shell's
        // process group. `eye` stops with it so the session stays coherent.
        if bytes.contains(&0x1a) {
            self.stop(&[libc::SIGTSTP]);
        }
        Ok(())
    }

    fn handle_signals(&mut self) -> Result<()> {
        for signo in self.signals.drain() {
            match signo {
                libc::SIGCHLD => self.reap(),
                libc::SIGWINCH => self.on_resize()?,
                libc::SIGINT | libc::SIGQUIT | libc::SIGTERM | libc::SIGHUP => {
                    self.record_signal(signo)?;
                    // Hand it to the session, not to ourselves: the user meant
                    // it for the program they are running.
                    let _ = tty::signal_group(self.pgid, signo);
                }
                libc::SIGTSTP => self.stop(&[libc::SIGTSTP]),
                libc::SIGCONT => self.resume(),
                // SIGPIPE is blocked so that a dead terminal surfaces as a
                // write error we can report, rather than killing the process.
                libc::SIGPIPE => {}
                _ => {}
            }
        }
        Ok(())
    }

    fn reap(&mut self) {
        let mut status: libc::c_int = 0;
        loop {
            let r = unsafe { libc::waitpid(self.child.id() as libc::pid_t, &mut status, libc::WNOHANG) };
            if r == self.child.id() as libc::pid_t {
                self.status = Some(ExitStatus::from_raw(status));
                return;
            }
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            return;
        }
    }

    fn on_resize(&mut self) -> Result<()> {
        let Ok(size) = tty::window_size(self.input) else {
            return Ok(());
        };
        // Setting the size on the pty is what makes the kernel deliver
        // SIGWINCH to the shell, exactly as a real terminal would.
        if self.pty.set_size(size).is_err() {
            return Ok(());
        }
        self.writer.push(&Event::Resize { t: self.elapsed(), size })?;
        Ok(())
    }

    fn record_signal(&mut self, signo: libc::c_int) -> Result<()> {
        self.writer.push(&Event::Signal {
            t: self.elapsed(),
            name: tty::short_signal_name(signo),
            number: signo,
        })?;
        Ok(())
    }

    /// Suspend the whole session, leaving the user's terminal usable.
    fn stop(&mut self, record: &[libc::c_int]) {
        for signo in record {
            let _ = self.record_signal(*signo);
        }
        // Hand the pty back to the kernel for the duration: a stopped `eye`
        // must not leave the user staring at a raw-mode terminal.
        self.raw.restore();

        // SIGTSTP is blocked; a stop signal that is blocked is discarded, so
        // it has to be unblocked for the instant of the stop.
        let mut only = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut only);
            libc::sigaddset(&mut only, libc::SIGTSTP);
        }
        tty::with_mask(&only, || unsafe {
            libc::kill(0, libc::SIGTSTP);
        });
        // Reached again only once SIGCONT arrives.
    }

    /// Come back from a stop.
    fn resume(&mut self) {
        let _ = self.record_signal(libc::SIGCONT);
        // The shell was stopped by the pty's line discipline, not by us.
        let _ = tty::signal_group(self.pgid, libc::SIGCONT);
        if !self.raw.is_active() {
            if let Ok(raw) = RawMode::enable(self.input, true) {
                self.raw = raw;
            }
        }
    }

    fn finish(&mut self) -> Result<RecordOutcome> {
        let t = self.elapsed();
        let (reason, code) = match self.status {
            Some(status) => match (status.code(), status.signal()) {
                (Some(code), _) => ("shell_exit", Some(code)),
                // Killed by a signal: record which one, since there is no
                // exit code to report.
                (None, Some(signo)) => {
                    self.writer.push(&Event::Signal {
                        t,
                        name: tty::short_signal_name(signo),
                        number: signo,
                    })?;
                    ("signal", None)
                }
                (None, None) => ("eof", None),
            },
            // The pty closed without us ever reaping the child.
            None => ("eof", None),
        };

        // A command the shell never reported finishing still gets an `Exit`,
        // so that every `Command` in a finished session has a verdict.
        self.events.clear();
        self.tracker.finish(t, code, &mut self.events);
        for event in self.events.drain(..) {
            self.writer.push(&event)?;
        }

        let events = self.writer.events();
        let path = self.writer.finish(reason, code)?;
        Ok(RecordOutcome { path, duration: t, exit_code: code, reason, events })
    }
}

/// `eye` moves the pty's foreground process group on purpose. Doing that from
/// a process that is not the terminal's foreground group raises `SIGTTOU`,
/// which would stop `eye` at the worst possible moment.
fn ignore_job_control_signals() {
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
    }
}

fn build_meta(inv: &shell::Invocation, size: Size, opts: &RecordOptions) -> Meta {
    let now = time::now_unix();
    Meta {
        format: FORMAT_NAME.into(),
        version: FORMAT_VERSION,
        tool: "eye".into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        created_at: time::format_rfc3339(now),
        created_at_unix: now,
        id: format!("{}-{}", time::now_unix_millis(), std::process::id()),
        title: opts.title.clone(),
        hostname: hostname(),
        cwd: opts
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| p.display().to_string()),
        shell: ShellInfo {
            path: inv.program.clone(),
            name: PathBuf::from(&inv.program)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            args: inv.args.clone(),
            integration: inv.integration.as_str().to_owned(),
        },
        terminal: TerminalInfo {
            term: std::env::var("TERM").unwrap_or_else(|_| "unknown".into()),
            colorterm: std::env::var("COLORTERM").ok(),
            size,
        },
        env: provenance_env(),
    }
}

/// A small, deliberate slice of the environment.
///
/// A full dump would be a privacy problem and would make two recordings of the
/// same session differ; these are the variables that actually explain what the
/// session looked like.
fn provenance_env() -> Vec<(String, String)> {
    const KEYS: &[&str] = &["TERM", "COLORTERM", "TERM_PROGRAM", "LANG", "LC_ALL", "SHELL"];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v)))
        .collect()
}

fn hostname() -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    let r = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
    if r != 0 {
        return None;
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_bytes();
    Some(String::from_utf8_lossy(bytes).into_owned())
}
