//! Decoding replay control keys.
//!
//! Replay reads keystrokes from the user's terminal while it is in raw mode,
//! which means every key arrives as raw bytes and multi-byte sequences can be
//! split across reads. This module turns that byte stream into [`Action`]s and
//! nothing else: no terminal handling, no timing policy, no I/O. That split is
//! why the interesting behaviour — a `←` arriving as `ESC [ D`, or half an
//! arrow key arriving in one read and the rest in the next — is testable
//! without a pty.
//!
//! Key bindings, chosen to match what people already expect from a session
//! player:
//!
//! | key | action |
//! |-----|--------|
//! | `q`, Ctrl-C | quit |
//! | Space | pause / resume |
//! | `.` | step forward one second (while paused) |
//! | `,` | step back one second (while paused) |
//! | `←` `→` | seek ∓/± [`SEEK_STEP`] seconds |
//! | Shift-`←` `→` | seek ∓/± [`SEEK_BIG_STEP`] seconds |
//! | `-` `_` / `+` `=` | slower / faster |
//! | `r` | restart |
//! | `0` | back to 1x |
//!
//! Anything else the user types is ignored: during replay the keyboard belongs
//! to `eye`, and silently treating an unbound key as something else would be
//! worse than dropping it.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// How far a plain arrow key seeks, in recorded seconds.
pub const SEEK_STEP: f64 = 5.0;
/// How far Shift-arrow seeks, in recorded seconds.
pub const SEEK_BIG_STEP: f64 = 30.0;
/// How far a single step key moves, in recorded seconds.
pub const STEP: f64 = 1.0;

/// A lone Escape is a real key, so an incomplete sequence is only abandoned
/// once a keypress could plausibly have finished arriving.
pub const ESC_TIMEOUT: Duration = Duration::from_millis(50);

/// Longest prefix we will hold while waiting for the rest of a sequence. Long
/// sequences exist (`CSI 1 ; 5 A`), but not 64-byte ones; a stream of `ESC`
/// bytes must not be able to grow this without bound.
const MAX_PENDING: usize = 16;

/// Something the user asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Leave replay. Handed back from the `ReplayOutcome` as the reason, so the
    /// CLI can tell "watched to the end" from "gave up at 40%".
    Quit,
    TogglePause,
    /// Move while paused: positive is forward.
    Step(f64),
    /// Relative seek in recorded seconds: negative is back.
    Seek(f64),
    Slower,
    Faster,
    Restart,
    /// Back to real time.
    NormalSpeed,
}

/// Incremental key decoder. Feed it whatever `read` returned; it holds partial
/// escape sequences between calls.
///
/// It also remembers where the bytes come from, so the player can ask "is there
/// anything left to read?" without carrying the descriptor around itself — and
/// so a key source that reaches end of file stops being polled rather than
/// spinning on a descriptor that is permanently ready and permanently empty.
#[derive(Debug, Default)]
pub struct KeyDecoder {
    pending: Vec<u8>,
    /// When the incomplete sequence started, for [`Self::expire`].
    since: Option<Instant>,
    /// `None` once detached, which reads as "no source" via [`Self::fd`].
    fd: Option<RawFd>,
}

impl KeyDecoder {
    /// A decoder with no input source. Fine for tests; nothing can be read.
    pub fn new() -> Self {
        KeyDecoder::default()
    }

    /// A decoder reading from `fd`.
    pub fn with_fd(fd: RawFd) -> Self {
        KeyDecoder { fd: Some(fd), ..KeyDecoder::default() }
    }

    /// The descriptor to poll, or `-1` when there is nothing left to poll.
    pub fn fd(&self) -> RawFd {
        self.fd.unwrap_or(-1)
    }

    /// Forget the input source. Idempotent.
    pub fn detach(&mut self) {
        self.fd = None;
        self.discard();
    }

    /// Decode a chunk of input, holding on to any partial sequence.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        for &byte in bytes {
            // Ctrl-C is never a sequence byte and always means "stop". Raw mode
            // keeps ISIG off, so this reaches us as a byte rather than as a
            // signal, and it must win even mid-arrow-key.
            if byte == 0x03 {
                self.discard();
                actions.push(Action::Quit);
                continue;
            }
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.begin();
                    continue;
                }
                if let Some(action) = single_byte(byte) {
                    actions.push(action);
                }
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() > MAX_PENDING {
                self.discard();
                continue;
            }
            match classify(&self.pending) {
                Decode::Incomplete => {}
                Decode::Done(action) => {
                    self.discard();
                    if let Some(action) = action {
                        actions.push(action);
                    }
                }
                Decode::Invalid => self.discard(),
            }
        }
        actions
    }

    /// Drop a pending sequence that has been waiting longer than a keypress
    /// takes.
    ///
    /// Without this, pressing Escape to get a menu to appear would swallow the
    /// next key. Returns whether anything was discarded.
    pub fn expire(&mut self, now: Instant) -> bool {
        let started = self.since.is_some_and(|s| now.saturating_duration_since(s) >= ESC_TIMEOUT);
        if started {
            self.discard();
        }
        started
    }

    /// True while an escape sequence is only half-received.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn begin(&mut self) {
        self.pending.clear();
        self.pending.push(0x1b);
        self.since = Some(Instant::now());
    }

    fn discard(&mut self) {
        self.pending.clear();
        self.since = None;
    }
}

/// Keys that stand alone, byte for byte.
fn single_byte(byte: u8) -> Option<Action> {
    Some(match byte {
        b'q' | 0x03 => Action::Quit,
        b' ' => Action::TogglePause,
        b'.' => Action::Step(STEP),
        b',' => Action::Step(-STEP),
        b'-' | b'_' => Action::Slower,
        b'+' | b'=' => Action::Faster,
        b'r' => Action::Restart,
        b'0' => Action::NormalSpeed,
        _ => return None,
    })
}

enum Decode {
    /// More bytes needed.
    Incomplete,
    /// Complete. Carries the action, or `None` for a sequence we do not bind.
    Done(Option<Action>),
    /// Not a sequence at all; drop what we have buffered.
    Invalid,
}

/// Classify a buffered escape sequence. `seq` always starts with `ESC`.
fn classify(seq: &[u8]) -> Decode {
    let body = match seq {
        [0x1b] => return Decode::Incomplete,
        [0x1b, b'O'] => return Decode::Incomplete,
        [0x1b, b'O', final_byte] => {
            // SS3: cursor keys in application mode, which some full-screen
            // programs switch into.
            return cursor_action(*final_byte, 1);
        }
        [0x1b, b'[', rest @ ..] => rest,
        _ => return Decode::Invalid,
    };

    // CSI: parameter bytes, then intermediate bytes, then one final byte.
    let Some(end) = body.iter().position(|b| (0x40..=0x7e).contains(b)) else {
        // Nothing before the final byte is outside the ranges a CSI sequence
        // may use, so this is either a valid prefix or junk that will never
        // complete.
        let valid = body.iter().all(|b| (0x20..=0x3f).contains(b));
        return if valid { Decode::Incomplete } else { Decode::Invalid };
    };
    let params = &body[..end];
    let final_byte = body[end];
    // Anything between the parameters and the final byte is an intermediate we
    // do not use; treat those sequences as unbound rather than guessing.
    if params.iter().any(|b| (0x20..=0x2f).contains(b)) {
        return Decode::Done(None);
    }
    let modifier = csi_modifier(params);
    cursor_action(final_byte, modifier)
}

/// Modifier from a CSI parameter list: `1;5D` is Ctrl-`←`, `1;2D` is Shift-`←`.
fn csi_modifier(params: &[u8]) -> u8 {
    let mut parts = params.split(|b| *b == b';').filter(|p| !p.is_empty());
    // Skip the leading parameter; it identifies the key in the `1;5` form.
    let _ = parts.next();
    parts.next().and_then(|m| std::str::from_utf8(m).ok()?.parse::<u8>().ok()).unwrap_or(1)
}

/// Map a cursor-key final byte plus modifier to an action.
fn cursor_action(final_byte: u8, modifier: u8) -> Decode {
    let step = match modifier {
        2 => SEEK_BIG_STEP,  // Shift
        _ => SEEK_STEP,
    };
    let action = match final_byte {
        b'C' => Action::Seek(step),
        b'D' => Action::Seek(-step),
        // Up, down, home, end and page keys have no binding: a player that
        // hijacks Home and End is a player that breaks the user's terminal.
        _ => return Decode::Done(None),
    };
    Decode::Done(Some(action))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(input: &[u8]) -> Vec<Action> {
        KeyDecoder::new().feed(input)
    }

    #[test]
    fn the_documented_single_keys_all_bind() {
        assert_eq!(actions(b"q"), vec![Action::Quit]);
        assert_eq!(actions(&[0x03]), vec![Action::Quit], "Ctrl-C must quit, not signal");
        assert_eq!(actions(b" "), vec![Action::TogglePause]);
        assert_eq!(actions(b"."), vec![Action::Step(STEP)]);
        assert_eq!(actions(b","), vec![Action::Step(-STEP)]);
        assert_eq!(actions(b"-"), vec![Action::Slower]);
        assert_eq!(actions(b"_"), vec![Action::Slower]);
        assert_eq!(actions(b"+"), vec![Action::Faster]);
        assert_eq!(actions(b"="), vec![Action::Faster]);
        assert_eq!(actions(b"r"), vec![Action::Restart]);
        assert_eq!(actions(b"0"), vec![Action::NormalSpeed]);
    }

    #[test]
    fn unbound_keys_are_ignored_rather_than_guessed_at() {
        assert_eq!(actions(b"hello"), vec![]);
        assert_eq!(actions(b"wasd"), vec![]);
        assert_eq!(actions(&[0x7f]), vec![]);
    }

    #[test]
    fn arrows_seek_by_five_seconds() {
        assert_eq!(actions(b"\x1b[C"), vec![Action::Seek(SEEK_STEP)]);
        assert_eq!(actions(b"\x1b[D"), vec![Action::Seek(-SEEK_STEP)]);
        assert_eq!(actions(b"\x1bOC"), vec![Action::Seek(SEEK_STEP)]);
        assert_eq!(actions(b"\x1bOD"), vec![Action::Seek(-SEEK_STEP)]);
    }

    #[test]
    fn shift_arrows_take_the_long_way_round() {
        assert_eq!(actions(b"\x1b[1;2C"), vec![Action::Seek(SEEK_BIG_STEP)]);
        assert_eq!(actions(b"\x1b[1;2D"), vec![Action::Seek(-SEEK_BIG_STEP)]);
    }

    #[test]
    fn ctrl_arrows_take_the_short_step() {
        assert_eq!(actions(b"\x1b[1;5C"), vec![Action::Seek(SEEK_STEP)]);
    }

    #[test]
    fn up_down_home_end_are_not_hijacked() {
        assert_eq!(actions(b"\x1b[A"), vec![]);
        assert_eq!(actions(b"\x1b[B"), vec![]);
        assert_eq!(actions(b"\x1b[H"), vec![]);
        assert_eq!(actions(b"\x1b[F"), vec![]);
        assert_eq!(actions(b"\x1b[5~"), vec![]);
        assert_eq!(actions(b"\x1b[1;5A"), vec![]);
    }

    #[test]
    fn a_sequence_split_across_reads_decodes_once() {
        let mut keys = KeyDecoder::new();
        assert_eq!(keys.feed(b"\x1b"), vec![]);
        assert!(keys.is_pending());
        assert_eq!(keys.feed(b"["), vec![]);
        assert!(keys.is_pending());
        assert_eq!(keys.feed(b"D"), vec![Action::Seek(-SEEK_STEP)]);
        assert!(!keys.is_pending());
    }

    #[test]
    fn a_sequence_and_a_key_in_one_read_both_come_out() {
        let mut keys = KeyDecoder::new();
        assert_eq!(keys.feed(b"\x1b[Dq"), vec![Action::Seek(-SEEK_STEP), Action::Quit]);
    }

    #[test]
    fn a_lone_escape_stops_swallowing_the_next_key() {
        let mut keys = KeyDecoder::new();
        let t0 = Instant::now();
        assert_eq!(keys.feed(b"\x1b"), vec![]);
        assert!(keys.is_pending());
        // Too soon: the sequence may still be arriving.
        assert!(!keys.expire(t0));
        // Past the keypress timeout: Escape was a key in its own right.
        assert!(keys.expire(t0 + ESC_TIMEOUT + Duration::from_millis(1)));
        assert!(!keys.is_pending());
        assert_eq!(keys.feed(b"q"), vec![Action::Quit]);
    }

    #[test]
    fn expiring_a_complete_stream_does_nothing() {
        let mut keys = KeyDecoder::new();
        keys.feed(b"q");
        let t0 = Instant::now();
        assert!(!keys.expire(t0 + ESC_TIMEOUT * 10));
    }

    #[test]
    fn nonsense_escape_sequences_are_dropped_not_buffered_forever() {
        let mut keys = KeyDecoder::new();
        assert_eq!(keys.feed(b"\x1b\x1b"), vec![]);
        assert!(!keys.is_pending(), "a doubled escape is not a sequence prefix");

        // A stream of digits and semicolons is a perfectly legal-looking CSI
        // prefix, so it stays "maybe incomplete" until the buffer gives up.
        let mut keys = KeyDecoder::new();
        let garbage: Vec<u8> = std::iter::once(0x1b)
            .chain(std::iter::once(b'['))
            .chain(std::iter::repeat(b'1').take(64))
            .collect();
        let actions = keys.feed(&garbage);
        assert!(!keys.is_pending(), "an over-long sequence must not grow the buffer");
        assert!(!actions.contains(&Action::Quit), "no action may come out of garbage");

        // What follows an abandoned sequence is read as fresh input rather
        // than thrown away with it: the decoder cannot know what the dropped
        // bytes were, and the user is more likely to have pressed a real key
        // than to be relying on a malformed sequence.
        let mut keys = KeyDecoder::new();
        let noise: Vec<u8> = std::iter::once(0x1b)
            .chain(std::iter::once(b'['))
            .chain(std::iter::repeat(b'1').take(64))
            .chain(std::iter::once(b'q'))
            .collect();
        assert_eq!(keys.feed(&noise), vec![Action::Quit]);
    }

    #[test]
    fn a_control_byte_in_the_middle_does_not_hide_a_later_key() {
        // Ctrl-C arriving mid-sequence still means "quit"; it must not be
        // swallowed as part of an arrow key.
        let mut keys = KeyDecoder::new();
        assert_eq!(keys.feed(b"\x1b[1;5"), vec![]);
        assert_eq!(keys.feed(&[0x03]), vec![Action::Quit]);
    }

    #[test]
    fn a_detached_decoder_has_nothing_to_poll() {
        let mut keys = KeyDecoder::with_fd(0);
        assert_eq!(keys.fd(), 0);
        keys.detach();
        assert_eq!(keys.fd(), -1, "a detached decoder must not ask to be polled");
        keys.detach();
        assert_eq!(keys.fd(), -1, "detaching twice is not a problem");
        // Decoding still works; only the source is gone.
        assert_eq!(keys.feed(b"q"), vec![Action::Quit]);
        assert_eq!(KeyDecoder::new().fd(), -1);
    }
}
