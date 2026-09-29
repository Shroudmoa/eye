//! Replay: put a recording back where it came from.
//!
//! The stored session is a byte stream with timestamps. Replay's whole job is
//! to write those bytes to a terminal again, in the right order, at the right
//! moments — and to hand the terminal back afterwards in a state the user's
//! shell can use.
//!
//! Four decisions shape the code:
//!
//! * **Replay does not interpret, it re-emits.** Nothing between the file and
//!   the terminal parses escape codes to decide what to draw; the recorded
//!   bytes go out verbatim. That is why colours, cursor tricks, animations and
//!   full-screen programs all work without `eye` understanding them, and why
//!   the [`Screen`] model is fed here purely for bookkeeping.
//! * **Seeking rebuilds, it does not invert.** Terminal output has no inverse,
//!   so a backwards seek re-emits everything from the start as fast as the
//!   terminal will take it. A forward seek only emits what was skipped.
//! * **The terminal is never left modified.** Replay changes modes — hiding the
//!   cursor, and whatever the recording itself changed — so leaving it means
//!   putting back every one of them, whether or not the recording ended
//!   cleanly. [`teardown_bytes`] is deliberately paranoid and runs on the error
//!   path too.
//! * **One clock, one loop, no runtime.** `poll` on a signal fd and on stdin,
//!   with the timeout computed from the next chunk's timestamp. There is
//!   exactly one task; an executor would add a dependency and remove no work.

pub mod clock;
pub mod keys;

use crate::event::Size;
use crate::session::{Item, Session};
use crate::storage::ReadError;
use crate::term::Screen;
use crate::tty::{self, RawMode, SignalFd};
use crate::util::fd::{self, FdWriter, NonBlocking};
use crate::util::time::format_offset;
use anyhow::{bail, Context, Result};
use clock::{faster, slower, Clock};
use keys::{Action, KeyDecoder};
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the progress line is redrawn while waiting.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// The longest single `poll` wait. Long enough that an idle session costs
/// nothing, short enough that a resize or a signal is noticed promptly.
const MAX_WAIT_MS: i32 = 250;

/// What the user asked for.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Recording to play.
    pub path: PathBuf,
    /// Playback rate, clamped to something usable.
    pub speed: f64,
    /// Recorded time to start at, in seconds.
    pub start: f64,
    /// Feed recorded geometry changes into the replay screen model.
    ///
    /// The user's real terminal is never resized: that would be `eye` moving
    /// furniture around in someone's workspace. Recorded resizes are only
    /// interesting to the screen model, so that teardown decisions and
    /// `--progress` reflect the geometry that was actually in effect.
    pub apply_resize: bool,
    /// Draw a one-line position/speed readout on the bottom row. Off by
    /// default because it does overwrite that row.
    pub progress: bool,
    /// Overrides for tests: where keys are read from and where output goes.
    pub input_fd: Option<RawFd>,
    pub output_fd: Option<RawFd>,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        ReplayOptions {
            path: PathBuf::new(),
            speed: 1.0,
            start: 0.0,
            apply_resize: true,
            progress: false,
            input_fd: None,
            output_fd: None,
        }
    }
}

impl ReplayOptions {
    pub fn input_fd(&self) -> RawFd {
        self.input_fd.unwrap_or(0)
    }
    pub fn output_fd(&self) -> RawFd {
        self.output_fd.unwrap_or(1)
    }
}

/// How a playback ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The recording played to its last event.
    End,
    /// The user pressed `q` or Ctrl-C.
    Quit,
    /// A signal arrived. Named, because forwarding it and dying without
    /// cleaning up is exactly the failure mode this module is built to avoid.
    Signal(String),
    /// The terminal went away mid-replay.
    Lost,
}

/// What a finished replay did.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayOutcome {
    pub path: PathBuf,
    /// Length of the recording.
    pub duration: f64,
    /// Recorded time the playhead reached.
    pub position: f64,
    pub chunks: u64,
    pub bytes: u64,
    pub reason: StopReason,
    /// Lines in the file that could not be parsed.
    pub malformed: u64,
}

/// Play a recording. Blocks until the recording ends or the user quits.
pub fn replay(opts: &ReplayOptions) -> Result<ReplayOutcome> {
    let input = opts.input_fd();
    let output = opts.output_fd();
    if !tty::is_tty(output) {
        bail!("eye replay needs a terminal on stdout to draw the session on");
    }

    let session = Session::load(&opts.path)
        .with_context(|| format!("read the recording at {}", opts.path.display()))?;

    // Signals arrive through a signalfd rather than handlers: the loop is in
    // the middle of writes, and a handler that touched the terminal from there
    // could tear the output in half.
    let mask = tty::blocked_mask(&[]);
    tty::block(&mask).context("block signals for signalfd")?;
    let signals = SignalFd::new(&mask).context("create a signalfd")?;

    // A terminal needs raw mode so keys do not echo and Ctrl-C arrives as a
    // byte. A pipe or a file does not, and setting raw mode on one would fail;
    // reading keys from a file is also how the tests drive this loop.
    let raw = if tty::is_tty(input) {
        Some(RawMode::enable(input, true).context("switch the terminal to raw mode")?)
    } else {
        None
    };
    // Only for the non-tty case: on a tty, raw mode's VMIN/VTIME already make a
    // read after `poll` non-blocking in practice, and `O_NONBLOCK` would leak
    // through the shared file description into the writes below.
    let nonblocking = if tty::is_tty(input) {
        None
    } else {
        Some(NonBlocking::new(input).context("make the key source non-blocking")?)
    };

    let size = tty::window_size(output).unwrap_or_else(|_| session.recorded_size());
    let mut player = Player::new(&session, input, output, size, opts);

    let played = player.run(&signals);
    // Modes while the terminal still has our settings; termios afterwards,
    // because `?1049l` and the attribute reset must not be rewritten on the
    // way out.
    player.teardown();
    drop(raw);
    drop(nonblocking);
    let _ = tty::unblock_all();

    let reason = played.context("write to the terminal during replay")?;
    Ok(ReplayOutcome {
        path: session.path.clone(),
        duration: session.duration,
        position: player.position,
        chunks: player.chunks,
        bytes: player.bytes,
        reason,
        malformed: session.malformed,
    })
}

/// The playback state machine.
struct Player<'a> {
    session: &'a Session,
    keys: KeyDecoder,
    out: FdWriter,
    screen: Screen,
    clock: Clock,
    /// Index of the next timeline item to draw.
    idx: usize,
    position: f64,
    chunks: u64,
    bytes: u64,
    apply_resize: bool,
    progress: bool,
    stop: Option<StopReason>,
}

impl<'a> Player<'a> {
    fn new(session: &'a Session, input: RawFd, output: RawFd, size: Size, opts: &ReplayOptions) -> Self {
        Player {
            session,
            keys: KeyDecoder::with_fd(input),
            out: FdWriter(output),
            screen: Screen::new(size),
            clock: Clock::new(opts.start.clamp(0.0, session.duration), opts.speed),
            idx: 0,
            position: opts.start.clamp(0.0, session.duration),
            chunks: 0,
            bytes: 0,
            apply_resize: opts.apply_resize,
            progress: opts.progress,
            stop: None,
        }
    }

    /// Wind up, whichever way this run ends. The first reason wins, so a
    /// terminal error after the user already pressed `q` is not reported twice.
    fn stop_with(&mut self, reason: StopReason) {
        if self.stop.is_none() {
            self.stop = Some(reason);
        }
    }

    /// True once there is nothing left to do.
    ///
    /// A paused playhead at the end of the recording is *not* finished: the
    /// user is looking at the last frame and may want to step back through it.
    fn finished(&self) -> bool {
        self.stop.is_some() || (self.idx >= self.session.timeline.len() && !self.clock.is_paused())
    }

    fn run(&mut self, signals: &SignalFd) -> io::Result<StopReason> {
        self.reset_frame()?;

        let mut fds = [
            libc::pollfd { fd: signals.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: -1, events: libc::POLLIN, revents: 0 },
        ];
        let mut buf = vec![0u8; fd::BUF];

        loop {
            if self.finished() {
                break;
            }
            self.drain_due()?;
            if self.finished() {
                break;
            }

            let timeout = self.next_timeout();
            fds[0].fd = signals.fd();
            fds[0].events = libc::POLLIN;
            fds[0].revents = 0;
            fds[1].fd = self.keys.fd();
            fds[1].events = libc::POLLIN;
            fds[1].revents = 0;

            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.stop_with(StopReason::Lost);
                return Ok(self.stop.take().expect("just set"));
            }
            if n == 0 {
                continue;
            }
            if fds[0].revents != 0 {
                self.handle_signals(signals);
            }
            if fds[1].revents != 0 {
                self.read_keys(&mut buf)?;
            }
        }

        self.draw_progress()?;
        Ok(self.stop.take().unwrap_or(StopReason::End))
    }

    fn handle_signals(&mut self, signals: &SignalFd) {
        for signo in signals.drain() {
            match signo {
                libc::SIGINT | libc::SIGTERM | libc::SIGQUIT | libc::SIGHUP => {
                    self.stop_with(StopReason::Signal(tty::signal_name(signo)));
                }
                // The real terminal resized itself; the screen model is the
                // only thing that needs to know, since the recorded bytes
                // were laid out for whatever size was current at the time.
                libc::SIGWINCH => {
                    if let Ok(size) = tty::window_size(self.out.0) {
                        self.screen.resize(size);
                    }
                }
                _ => {}
            }
        }
    }

    /// How long to wait before the next chunk is due, in milliseconds.
    ///
    /// A paused clock gets an infinite wait when nothing needs painting: there
    /// is nothing to wait *for* until the user presses something.
    fn next_timeout(&self) -> i32 {
        let cap = if self.progress { PROGRESS_TICK.as_millis() as i32 } else { MAX_WAIT_MS };
        if self.clock.is_paused() {
            return if self.progress { cap } else { -1 };
        }
        let due = self
            .clock
            .delay_until(self.session.timeline[self.idx].t(), Instant::now())
            .map(|d| d.as_millis().min(u128::from(cap as u32)) as i32)
            .unwrap_or(0);
        due.min(cap)
    }

    /// Draw everything the playhead has reached.
    fn drain_due(&mut self) -> io::Result<()> {
        let pos = self.clock.position();
        while let Some(item) = self.session.timeline.get(self.idx) {
            if item.t() > pos {
                break;
            }
            self.emit(item)?;
            self.idx += 1;
        }
        self.position = pos;
        self.draw_progress()
    }

    /// Write one timeline item to the terminal and to the screen model.
    fn emit(&mut self, item: &Item) -> io::Result<()> {
        match item {
            Item::Output { range, .. } => {
                let bytes = &self.session.output()[range.clone()];
                // Bookkeeping first: a terminal that dies mid-replay should
                // cost us the picture, not the state needed to tidy it up.
                self.screen.feed(bytes);
                self.chunks += 1;
                self.bytes += bytes.len() as u64;
                self.out.write_all(bytes)?;
            }
            Item::Resize { size, .. } => {
                if self.apply_resize {
                    self.screen.resize(*size);
                }
            }
        }
        Ok(())
    }

    fn read_keys(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let fd = self.keys.fd();
        if fd < 0 {
            return Ok(());
        }
        match fd::read_once(fd, buf) {
            Ok(0) => self.lose_keys(),
            Ok(n) => {
                let actions = self.keys.feed(&buf[..n]);
                self.keys.expire(Instant::now());
                for action in actions {
                    self.dispatch(action)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Nothing right now, on a source that cannot say so at `poll` time.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.lose_keys(),
        }
        Ok(())
    }

    /// The key source has ended.
    ///
    /// Stop polling it: a descriptor that is permanently ready and permanently
    /// empty would otherwise spin the loop. If playback is paused at that
    /// moment, stop outright rather than wait forever — the only thing a paused
    /// player can wait for is a keypress, and no keypress is now possible.
    fn lose_keys(&mut self) {
        self.keys.detach();
        if self.clock.is_paused() {
            self.stop_with(StopReason::Quit);
        }
    }

    fn dispatch(&mut self, action: Action) -> io::Result<()> {
        let now = Instant::now();
        match action {
            Action::Quit => self.stop_with(StopReason::Quit),
            Action::TogglePause => {
                self.clock.toggle_pause(now);
            }
            Action::Step(dt) => {
                // Stepping only moves a paused playhead, and drawing what it
                // revealed is the point of stepping at all.
                if self.clock.step(dt, now) {
                    self.seek_to(self.clock.position(), now)?;
                }
            }
            Action::Seek(delta) => self.seek_to(self.clock.position() + delta, now)?,
            Action::Restart => self.seek_to(0.0, now)?,
            Action::Slower => self.clock.set_speed(slower(self.clock.speed()), now),
            Action::Faster => self.clock.set_speed(faster(self.clock.speed()), now),
            Action::NormalSpeed => self.clock.set_speed(1.0, now),
        }
        // A pause must not redraw behind the user's back; anything else may
        // have just revealed chunks that are now due.
        if !self.clock.is_paused() {
            self.drain_due()?;
        }
        self.position = self.clock.position();
        self.draw_progress()
    }

    /// Move the playhead, drawing whatever that implies.
    ///
    /// A backwards seek cannot un-draw, so the screen is reset and the session
    /// re-emitted from the top; a forward seek emits only the part that was
    /// skipped.
    fn seek_to(&mut self, target: f64, now: Instant) -> io::Result<()> {
        let target = target.clamp(0.0, self.session.duration);
        if self.session.items_upto(target) < self.idx {
            self.reset_frame()?;
            self.idx = 0;
        }
        while self.idx < self.session.items_upto(target) {
            self.emit(&self.session.timeline[self.idx])?;
            self.idx += 1;
        }
        self.clock.seek(target, now);
        self.position = target;
        self.draw_progress()
    }

    /// Back to a blank screen, with nothing left in any mode.
    fn reset_frame(&mut self) -> io::Result<()> {
        let bytes = reset_frame_bytes(self.screen.in_alt_screen());
        self.out.write_all(&bytes)?;
        self.screen = Screen::new(self.screen.size());
        Ok(())
    }

    /// The status line, drawn on the bottom row and immediately undone.
    ///
    /// The cursor goes back where the recording left it, so the only lasting
    /// trace is the cells the line covered — which is why this is opt-in.
    fn draw_progress(&mut self) -> io::Result<()> {
        if !self.progress || self.stop.is_some() {
            return Ok(());
        }
        let line = self.status_line();
        let cursor = self.screen.cursor();
        let mut buf = String::with_capacity(line.len() + 48);
        buf.push_str(&format!("\x1b[{};1H\x1b[7m", self.screen.size().rows));
        buf.push_str(&line);
        buf.push_str(&format!("\x1b[0m\x1b[{};{}H", cursor.row + 1, cursor.col + 1));
        self.out.write_all(buf.as_bytes())
    }

    fn status_line(&self) -> String {
        let speed = self.clock.speed();
        // One decimal, so `0.25x` reads as `0.25x` rather than being rounded
        // into something that is not on the speed ladder.
        let speed_text = if (speed - 1.0).abs() < 0.005 {
            "1x".to_string()
        } else {
            let rounded = (speed * 100.0).round() / 100.0;
            if (rounded - rounded.round()).abs() < 0.005 {
                format!("{}x", rounded.round() as i64)
            } else {
                format!("{rounded}x")
            }
        };
        let mode = if self.clock.is_paused() { "paused" } else { "playing" };
        format!(
            " eye  {} / {}   {}  {}   q quit ",
            format_offset(self.position),
            format_offset(self.session.duration),
            speed_text,
            mode,
        )
    }

    /// Put the terminal back the way it was found.
    fn teardown(&mut self) {
        let bytes = teardown_bytes(self.screen.in_alt_screen());
        let _ = self.out.write_all(&bytes);
    }
}

/// The sequence that starts a replay from a blank slate.
///
/// The recorded stream begins wherever the session began, which on a real
/// terminal is not the top-left corner: whatever the user's shell had already
/// drawn is still there, and the replayed prompt would land on top of it.
pub fn reset_frame_bytes(in_alt_screen: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    if in_alt_screen {
        // Leave the alternate buffer first, so the buffer being cleared is the
        // one the user is about to see.
        out.extend_from_slice(b"\x1b[?1049l");
    }
    out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[3J\x1b[H");
    out
}

/// The sequence that hands the terminal back.
///
/// Deliberately longer than the minimum. A recording can leave the terminal in
/// *any* of these states — the shell that produced it, or the full-screen
/// program it last ran — and every one of them left as-is would make the
/// user's shell misbehave after replay. Everything here is idempotent and
/// cheap.
pub fn teardown_bytes(in_alt_screen: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    if in_alt_screen {
        // Restores the primary buffer, the cursor position and the attributes
        // saved when the recording entered the alternate screen.
        out.extend_from_slice(b"\x1b[?1049l");
    }
    // Geometry and modes a program may have changed and not put back.
    out.extend_from_slice(b"\x1b[r"); // back to a full-screen scroll region
    out.extend_from_slice(b"\x1b[?6l"); // origin mode off
    out.extend_from_slice(b"\x1b[?7h"); // autowrap on
    out.extend_from_slice(b"\x1b[?1l\x1b>"); // numeric keypad, application off
    out.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"); // mouse reporting
    out.extend_from_slice(b"\x1b[?2004l"); // bracketed paste
    // Attributes and cursor last, so nothing is left styled.
    out.extend_from_slice(b"\x1b[0m");
    out.extend_from_slice(b"\x1b[?25h");
    out
}

/// Load a recording without playing it.
///
/// The tests and `eye inspect` want exactly this check, so there is one
/// implementation of "is this file playable?".
pub fn load(path: &Path) -> Result<Session, ReadError> {
    Session::load(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Bytes, Event, Stream};
    use crate::pty::Pty;
    use crate::testsupport;
    use std::io::Write as _;
    use std::os::fd::{AsRawFd, FromRawFd};

    /// A recording of `(recorded time, bytes)` pairs.
    fn write_session(name: &str, chunks: &[(f64, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eye-r-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.eye");
        let mut w = crate::storage::SessionWriter::create(&path, testsupport::meta()).unwrap();
        w.push(&Event::SessionStart { t: 0.0, size: Size::new(80, 24) }).unwrap();
        for (t, text) in chunks {
            w.push(&Event::Output {
                t: *t,
                data: Bytes::from(text.as_bytes().to_vec()),
                stream: Stream::Tty,
            })
            .unwrap();
        }
        w.finish("shell_exit", Some(0)).unwrap();
        path
    }

    /// A pty standing in for the user's terminal, plus the keystrokes to feed
    /// replay from.
    ///
    /// Deliberately single-threaded: a threaded reader would deadlock against
    /// the replay loop's `libc::write`, since a blocking write holds the GIL
    /// while the reader waits for the lock. Reading the pty *after* replay
    /// returns gets the same bytes with none of that.
    struct Fake {
        pty: Pty,
        keys: std::fs::File,
        written: Vec<u8>,
    }

    impl Fake {
        fn new(name: &str, keys: &[u8]) -> Fake {
            let dir = std::env::temp_dir().join(format!("eye-r-{}-{}", std::process::id(), name));
            std::fs::create_dir_all(&dir).unwrap();
            let mut file = std::fs::File::create(dir.join("keys")).unwrap();
            file.write_all(keys).unwrap();
            drop(file);

            let pty = Pty::open().unwrap();
            tty::set_window_size(pty.master_fd(), Size::new(80, 24)).unwrap();
            // Echo is off, or the keys typed at the player come back at it.
            let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(pty.master_fd(), &mut attrs) }, 0);
            attrs.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL | libc::ICANON);
            attrs.c_cc[libc::VMIN] = 1;
            attrs.c_cc[libc::VTIME] = 0;
            assert_eq!(unsafe { libc::tcsetattr(pty.master_fd(), libc::TCSANOW, &attrs) }, 0);
            // Non-blocking so draining stops at the end instead of parking.
            fd::set_nonblocking(pty.master_fd(), true).unwrap();

            Fake { pty, keys: std::fs::File::open(dir.join("keys")).unwrap(), written: Vec::new() }
        }

        fn run(&mut self, mut opts: ReplayOptions) -> Result<ReplayOutcome> {
            opts.input_fd = Some(self.keys.as_raw_fd());
            opts.output_fd = Some(self.pty.slave_fd());
            let outcome = replay(&opts);
            self.drain();
            outcome
        }

        /// Replay with this fake's pty as the screen and `opts` otherwise.
        fn run_with(&mut self, mut opts: ReplayOptions, output: RawFd) -> Result<ReplayOutcome> {
            opts.input_fd = Some(self.keys.as_raw_fd());
            opts.output_fd = Some(output);
            let outcome = replay(&opts);
            self.drain();
            outcome
        }

        /// A descriptor that is definitely not a terminal: a pipe's read end.
        fn not_a_tty() -> (RawFd, std::fs::File) {
            let mut fds = [0i32; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            (file.as_raw_fd(), file)
        }

        fn drain(&mut self) {
            let mut buf = [0u8; 64 * 1024];
            while let Ok(n) = fd::read_once(self.pty.master_fd(), &mut buf) {
                if n == 0 {
                    break;
                }
                self.written.extend_from_slice(&buf[..n]);
            }
        }

        /// What replay actually put on the "screen".
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.written).into_owned()
        }
    }

    fn opts(path: PathBuf) -> ReplayOptions {
        ReplayOptions { path, ..Default::default() }
    }

    #[test]
    fn the_frame_starts_from_a_blank_screen() {
        assert_eq!(reset_frame_bytes(false), b"\x1b[0m\x1b[2J\x1b[3J\x1b[H");
        assert_eq!(
            reset_frame_bytes(true),
            b"\x1b[?1049l\x1b[0m\x1b[2J\x1b[3J\x1b[H",
            "leaving the alternate screen before clearing avoids erasing the wrong buffer",
        );
    }

    #[test]
    fn teardown_undoes_every_mode_a_recording_could_leave_behind() {
        let seq = String::from_utf8(teardown_bytes(false)).unwrap();
        for expected in [
            "\x1b[r",      // scroll region
            "\x1b[?6l",    // origin mode
            "\x1b[?7h",    // autowrap
            "\x1b[?2004l", // bracketed paste
            "\x1b[?1006l", // mouse reporting
            "\x1b[0m",     // attributes
            "\x1b[?25h",   // cursor visible
        ] {
            assert!(seq.contains(expected), "teardown is missing {expected:?}");
        }
        assert!(
            !String::from_utf8(teardown_bytes(false)).unwrap().contains("?1049"),
            "a session that never used the alternate screen must not switch out of it"
        );
        assert!(String::from_utf8(teardown_bytes(true)).unwrap().contains("\x1b[?1049l"));
        // The cursor comes back after the attributes it might otherwise be
        // styled by.
        let seq = String::from_utf8(teardown_bytes(false)).unwrap();
        assert!(seq.find("\x1b[0m").unwrap() < seq.find("\x1b[?25h").unwrap());
    }

    #[test]
    fn replaying_ends_when_the_recording_ends() {
        let path = write_session("end", &[(0.0, "one"), (0.02, "two")]);
        let mut f = Fake::new("end", b"");
        let outcome = f.run(opts(path)).unwrap();
        assert_eq!(outcome.reason, StopReason::End);
        assert!((outcome.duration - 0.02).abs() < 1e-6, "{outcome:?}");
        // The playhead is read from the wall clock, so it lands on the last
        // chunk plus whatever elapsed while the terminal took the bytes.
        assert!(
            outcome.position >= outcome.duration,
            "playback must not stop short of the end it reported"
        );
        assert_eq!(outcome.chunks, 2);
        assert_eq!(outcome.bytes, 6);
        let text = f.text();
        assert!(text.contains("one") && text.contains("two"), "{text:?}");
        assert!(text.starts_with(&String::from_utf8(reset_frame_bytes(false)).unwrap()));
    }

    #[test]
    fn quitting_early_stops_early() {
        let path = write_session("quit", &[(0.0, "quick"), (600.0, "much later")]);
        let mut f = Fake::new("quit", b"q");
        let outcome = f.run(opts(path)).unwrap();
        assert_eq!(outcome.reason, StopReason::Quit);
        assert_eq!(outcome.chunks, 1);
        // "Early" is measured against the six hundred seconds never played,
        // not to the microsecond: the point is that it stopped.
        assert!(outcome.position < 1.0, "{outcome:?}");
        assert!(f.text().contains("quick"));
        assert!(!f.text().contains("much later"));
    }

    #[test]
    fn ctrl_c_quits_too() {
        let path = write_session("ctrlc", &[(0.0, "a"), (600.0, "b")]);
        let mut f = Fake::new("ctrlc", &[0x03]);
        assert_eq!(f.run(opts(path)).unwrap().reason, StopReason::Quit);
    }

    #[test]
    fn starting_late_draws_the_skipped_part_at_once_and_saves_the_waiting() {
        // Seeking forward re-emits what it skipped, instantly. That is the whole
        // mechanism: the screen has to end up in the state it was in at 1.5s,
        // and terminal output has no "jump to" of its own. What `--start` saves
        // is the wall-clock time, not the bytes.
        let path = write_session("start", &[(0.0, "intro"), (1.0, "middle"), (2.0, "end")]);
        let mut f = Fake::new("start", b"");
        let began = Instant::now();
        let outcome =
            f.run(ReplayOptions { path, start: 1.5, speed: 1.0, ..Default::default() }).unwrap();
        let elapsed = began.elapsed();

        assert_eq!(outcome.reason, StopReason::End);
        assert_eq!(outcome.chunks, 3, "everything up to the end must be on screen");
        let text = f.text();
        assert!(text.contains("intro") && text.contains("middle") && text.contains("end"));
        // 0.5 recorded seconds remain, so a real-time replay must take about
        // half a second and not the 2s a start-at-zero run would.
        assert!(
            elapsed < Duration::from_millis(1_500),
            "the skipped second should not have been waited out: {elapsed:?}"
        );
    }

    #[test]
    fn starting_at_or_past_the_end_just_finishes() {
        let path = write_session("past", &[(0.0, "a"), (1.0, "b")]);
        let mut f = Fake::new("past", b"");
        let outcome = f.run(ReplayOptions { path, start: 99.0, ..Default::default() }).unwrap();
        assert_eq!(outcome.reason, StopReason::End);
        assert_eq!(outcome.chunks, 2, "everything is behind the playhead");
        assert!(
            (outcome.position - 1.0).abs() < 0.5,
            "the playhead lands at the end, give or take a scheduling hiccup: {outcome:?}"
        );
    }

    #[test]
    fn starting_inside_a_chunk_draws_that_chunk_whole() {
        // A chunk is one write(2) the program made; there is no such thing as
        // showing half of it, and inventing one would be a lie about timing.
        let path = write_session("atomic", &[(0.0, "whole"), (0.10, "later")]);
        let mut f = Fake::new("atomic", b"");
        let outcome =
            f.run(ReplayOptions { path, start: 0.01, speed: 8.0, ..Default::default() }).unwrap();
        assert_eq!(outcome.chunks, 2, "the chunk at t=0 is entirely before t=0.01");
        assert!(f.text().contains("whole"));
    }

    #[test]
    fn the_terminal_is_restored_even_when_the_user_quits_mid_session() {
        let path = write_session("restore", &[(0.0, "x")]);
        let mut f = Fake::new("restore", b"q");
        f.run(opts(path)).unwrap();
        let seq = String::from_utf8(teardown_bytes(false)).unwrap();
        assert!(f.text().contains(&seq), "teardown must go out on the quit path too");
    }

    #[test]
    fn a_session_that_ends_in_a_full_screen_program_hands_the_primary_buffer_back() {
        // A second chunk ten seconds in keeps replay alive long enough for the
        // `q` to arrive before the recording ends.
        let path = write_session(
            "altscreen",
            &[(0.0, "\x1b[?1049h\x1b[2J\x1b[Hinside vim"), (600.0, "later")],
        );
        let mut f = Fake::new("altscreen", b"q");
        let outcome = f.run(opts(path)).unwrap();
        assert_eq!(outcome.reason, StopReason::Quit);
        let text = f.text();
        assert!(text.contains("inside vim"));
        // One `?1049l` on the way out, and not one from the reset frame too:
        // that frame only leaves the alternate screen if one was in use, and
        // the user was in the primary buffer when replay started.
        assert_eq!(text.matches("\x1b[?1049l").count(), 1, "{text:?}");
        assert!(text.contains(&String::from_utf8(teardown_bytes(true)).unwrap()));
    }

    #[test]
    fn a_paused_player_stops_when_its_keys_run_out() {
        // Paused with nothing left to press: an interactive terminal never
        // hits this, but a piped key source does, and waiting forever on a key
        // that cannot arrive would hang `eye` rather than end it.
        let path = write_session("stall", &[(0.0, "a"), (600.0, "b")]);
        let mut f = Fake::new("stall", b" ");
        let outcome = f.run(ReplayOptions { path, speed: 8.0, ..Default::default() }).unwrap();
        assert_eq!(outcome.reason, StopReason::Quit);
        assert!(outcome.chunks < 2, "playback must not run past the pause");
    }

    #[test]
    fn seeking_backwards_replays_from_the_top() {
        // Terminal output cannot be un-drawn, so going back means clearing the
        // screen and re-emitting from the start. The playhead has to actually
        // be past a chunk for that to be meaningful, so: pause, step over the
        // first chunk, restart, resume.
        let path = write_session("seek", &[(0.5, "FIRST"), (4.0, "SECOND")]);
        let mut f = Fake::new("seek", b" .r ");
        let outcome =
            f.run(ReplayOptions { path, speed: 8.0, ..Default::default() }).unwrap();
        assert_eq!(outcome.reason, StopReason::End, "it plays on to the end after restarting");
        assert!((outcome.position - 4.0).abs() < 0.5, "{outcome:?}");

        let text = f.text();
        assert_eq!(
            text.matches("FIRST").count(),
            2,
            "the step and the restart must each have drawn it: {text:?}"
        );
        let frame = String::from_utf8(reset_frame_bytes(false)).unwrap();
        assert_eq!(
            text.matches(&frame).count(),
            2,
            "the opening frame and the restart's must both be there: {text:?}"
        );
        // The step is a forward move and must not clear anything.
        assert_eq!(text.matches("SECOND").count(), 1);
    }

    #[test]
    fn seeking_forwards_only_draws_what_it_skipped() {
        let path = write_session("fwd", &[(0.0, "FIRST"), (2.0, "SECOND")]);
        let mut f = Fake::new("fwd", b" \x1b[C");
        // Pause as soon as the first chunk is up, then skip five seconds.
        let outcome = f.run(ReplayOptions { path, speed: 8.0, ..Default::default() }).unwrap();
        assert_eq!(outcome.chunks, 2, "the skip must land the second chunk");
        let text = f.text();
        assert_eq!(
            text.matches("FIRST").count(),
            1,
            "a forward seek has nothing to undo, so nothing is redrawn: {text:?}"
        );
        assert!(text.contains("SECOND"));
    }

    #[test]
    fn replay_refuses_to_run_without_a_terminal_to_draw_on() {
        // The pty slave path is itself a terminal, so "not a tty" needs a real
        // pipe; writing the session into it would succeed and prove nothing.
        let (notty, _keep) = Fake::not_a_tty();
        let path = write_session("notty", &[(0.0, "x")]);
        let mut f = Fake::new("notty", b"");
        let err = f.run_with(opts(path), notty).unwrap_err().to_string();
        assert!(err.contains("terminal"), "{err}");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let mut f = Fake::new("missing", b"");
        let err = f
            .run(opts(PathBuf::from("/nonexistent/eye/nope.eye")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope.eye"), "{err}");
    }

    #[test]
    fn load_is_the_check_the_play_path_uses() {
        let path = write_session("load", &[(0.0, "x")]);
        assert_eq!(load(&path).unwrap().meta.shell.name, "bash");
    }

    #[test]
    fn the_status_line_reports_position_speed_and_pause_state() {
        let session = Session::load(&write_session("status", &[(0.0, "a")])).unwrap();
        let mut p = Player::new(&session, -1, 1, Size::new(80, 24), &ReplayOptions::default());
        p.position = 12.5;
        let line = p.status_line();
        assert!(line.contains("00:12.500"), "{line}");
        assert!(line.contains("1x"));
        assert!(line.contains("playing"));
        p.clock.pause(Instant::now());
        assert!(p.status_line().contains("paused"));
        p.clock.set_speed(8.0, Instant::now());
        assert!(p.status_line().contains("8x"));
    }

    #[test]
    fn pausing_at_the_end_keeps_replay_open_for_looking_around() {
        // Paused at the very end, the player must not exit on its own: there is
        // nothing left to draw, but the user has not said they are finished.
        let session = Session::load(&write_session("hold", &[(0.0, "a")])).unwrap();
        let mut p = Player::new(&session, -1, 1, Size::new(80, 24), &ReplayOptions::default());
        p.idx = session.timeline.len();
        assert!(p.finished(), "playing past the last chunk is finished");
        p.clock.pause(Instant::now());
        assert!(!p.finished(), "a paused playhead at the end is still interactive");
        p.clock.resume(Instant::now());
        assert!(p.finished());
        p.clock.pause(Instant::now());
        p.stop_with(StopReason::Quit);
        assert!(p.finished(), "stopping wins over being paused");
    }
}
