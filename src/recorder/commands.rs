//! Recovering command boundaries and exit codes from a byte stream.
//!
//! A PTY gives you bytes and nothing else. Two independent mechanisms turn
//! those bytes back into commands, and this module runs both:
//!
//! * **OSC 133 marks** emitted by an integrated shell. When present they are
//!   authoritative: they say exactly where a command started and what it
//!   returned.
//! * **Echo reconstruction.** `eye` sees every keystroke on its way to the pty,
//!   so it can rebuild the line the user submitted, then confirm it against
//!   what the terminal echoed back. This works with any shell at all, but it
//!   cannot recover exit codes — the PTY does not carry them.
//!
//! The tracker is deliberately conservative: if the echo does not match what
//! was typed, no command event is emitted. A missing command is a smaller lie
//! than an invented one.

use crate::event::{Bytes, CommandSource, Event};

/// One OSC 133 shell mark.
///
/// `A` prompt start, `B` prompt/line accepted, `C` command start, `D` prompt
/// end. Only `C` and `D` are load-bearing: they are the one pair every
/// integrated shell emits at exactly the same two instants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub kind: u8,
    /// Exit status, carried by `D`. Other kinds carry shell-specific details
    /// (fish puts the command line in `C;cmdline_url=...`) which are ignored.
    pub status: Option<i32>,
}

/// Streams a byte chunk, separating OSC 133 marks from printable text.
#[derive(Debug, Default)]
pub struct MarkScanner {
    state: ScanState,
    osc: Vec<u8>,
    utf8: Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

/// Longest OSC payload we will buffer while looking for a terminator.
const MAX_OSC: usize = 4 * 1024;

/// U+FFFD. A session is a record of what happened; where the bytes were not
/// valid text we say so rather than guessing.
const REPLACEMENT: char = '\u{fffd}';

/// Incremental UTF-8 decoder.
///
/// A keystroke or a redraw can split a multi-byte scalar anywhere, and this
/// scanner is fed whatever `read(2)` happened to return. Buffering a partial
/// scalar between chunks is therefore required, not an optimisation.
#[derive(Debug, Default)]
struct Utf8 {
    buf: [u8; 4],
    len: usize,
    need: usize,
}

/// What [`Utf8::push`] made of a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextResult {
    /// Part of a scalar; wait for the rest.
    Pending,
    /// A scalar completed.
    Done(char),
    /// The buffered scalar was abandoned. The caller should report a
    /// replacement character and re-examine the byte as a fresh one.
    Restart,
}

impl Utf8 {
    /// True when no scalar is half-read.
    fn is_idle(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, b: u8) -> TextResult {
        if self.len == 0 {
            self.need = match b {
                // Reject overlong and out-of-range lead bytes outright; the
                // continuation check below cannot catch those.
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return TextResult::Done(REPLACEMENT),
            };
            self.buf[0] = b;
            self.len = 1;
            return TextResult::Pending;
        }
        if !(0x80..=0xbf).contains(&b) {
            self.reset();
            return TextResult::Restart;
        }
        self.buf[self.len] = b;
        self.len += 1;
        if self.len < self.need {
            return TextResult::Pending;
        }
        let bytes = &self.buf[..self.len];
        let decoded = std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(REPLACEMENT);
        self.reset();
        TextResult::Done(decoded)
    }

    /// Consume a scalar that never finished arriving.
    fn take_incomplete(&mut self) -> Option<char> {
        (self.len > 0).then(|| {
            self.reset();
            REPLACEMENT
        })
    }

    fn reset(&mut self) {
        self.len = 0;
        self.need = 0;
    }
}

impl MarkScanner {
    /// Split `bytes` into plain text and OSC 133 marks.
    ///
    /// Control sequences other than OSC 133 are dropped, so the caller sees the
    /// text a person would have read.
    pub fn feed(&mut self, bytes: &[u8], text: &mut String, marks: &mut Vec<Mark>) {
        for &b in bytes {
            match self.state {
                ScanState::Ground => match b {
                    0x1b => self.state = ScanState::Escape,
                    b'\r' | b'\n' => text.push('\n'),
                    b'\t' => text.push('\t'),
                    0x08 => {
                        text.pop();
                    }
                    0x00..=0x07 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f => {}
                    _ => self.push_text(b, text),
                },
                ScanState::Escape => match b {
                    b'[' => self.state = ScanState::Csi,
                    b']' => {
                        self.state = ScanState::Osc;
                        self.osc.clear();
                    }
                    // Two-character escape: nothing visible in it.
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = ScanState::Ground;
                    }
                }
                ScanState::Osc => match b {
                    0x07 => self.finish_osc(marks),
                    0x1b => self.state = ScanState::OscEscape,
                    _ => {
                        if self.osc.len() < MAX_OSC {
                            self.osc.push(b);
                        }
                    }
                },
                ScanState::OscEscape => {
                    self.state = ScanState::Osc;
                    if b == b'\\' {
                        self.finish_osc(marks);
                    } else if self.osc.len() < MAX_OSC {
                        self.osc.push(b);
                    }
                }
            }
        }
    }

    /// Flush a UTF-8 scalar that was cut short by the end of the stream.
    ///
    /// Chunks are handled incrementally, so a scalar split across two reads is
    /// normal; only a session that ends mid-character loses something here.
    pub fn flush(&mut self, text: &mut String) {
        if let Some(c) = self.utf8.take_incomplete() {
            text.push(c);
        }
    }

    /// Append one byte of visible text, decoding UTF-8 across chunk boundaries.
    fn push_text(&mut self, b: u8, text: &mut String) {
        loop {
            // The ASCII shortcut is only valid when no scalar is half-read:
            // otherwise a stray `a` would hide the fact that a lead byte was
            // left dangling.
            if b.is_ascii() && self.utf8.is_idle() {
                text.push(b as char);
                return;
            }
            match self.utf8.push(b) {
                // A continuation byte was required but something else turned
                // up. Report the loss, then reconsider this byte as a fresh
                // one so it is not swallowed too.
                TextResult::Restart => text.push(REPLACEMENT),
                TextResult::Pending => return,
                TextResult::Done(c) => {
                    text.push(c);
                    return;
                }
            }
        }
    }

    fn finish_osc(&mut self, marks: &mut Vec<Mark>) {
        self.state = ScanState::Ground;
        let payload = std::mem::take(&mut self.osc);
        let Ok(text) = std::str::from_utf8(&payload) else {
            return;
        };
        let Some(rest) = text.strip_prefix("133;") else {
            return;
        };
        let (kind, status) = match rest.split_once(';') {
            Some((k, s)) => (k.trim(), s.trim().parse::<i32>().ok()),
            None => (rest.trim(), None),
        };
        let Some(kind) = kind.as_bytes().first().copied() else {
            return;
        };
        matches!(
            kind,
            b'A' | b'B' | b'C' | b'D'
        )
        .then(|| marks.push(Mark { kind, status }));
    }
}

/// Builds command and exit events from the two input streams.
#[derive(Debug)]
pub struct CommandTracker {
    /// Whether OSC 133 marks are expected from this shell.
    integration: bool,
    next_id: u64,
    /// What the user has typed so far on the current input line.
    typed: String,
    /// Partial UTF-8 scalar inside `typed`, across read boundaries.
    typed_utf8: Utf8,
    /// Whether `typed` is still trustworthy. History recall and similar
    /// keybindings produce edits this model cannot follow, so we give up rather
    /// than invent a command.
    typed_trustworthy: bool,
    /// The last line submitted, for integration marks to attach text to.
    last_submitted: Option<String>,
    /// A submitted line still waiting for its echo to confirm it.
    pending: Option<Pending>,
    /// Plain text the terminal printed since the last line break.
    echo_line: String,
    /// Command id currently executing.
    open: Option<u64>,
    scanner: MarkScanner,
    scratch: String,
    /// Staging buffer for walking `scratch` while mutating other fields.
    chars: Vec<char>,
}

#[derive(Debug)]
struct Pending {
    id: u64,
    text: String,
}

impl CommandTracker {
    /// `integration` should match what [`crate::recorder::shell::Shell`]
    /// decided; when it is wrong the echo path still works, it just also emits
    /// `Exit` events with no status.
    pub fn new(integration: bool) -> Self {
        CommandTracker {
            integration,
            next_id: 0,
            typed: String::new(),
            typed_utf8: Utf8::default(),
            typed_trustworthy: true,
            last_submitted: None,
            pending: None,
            echo_line: String::new(),
            open: None,
            scanner: MarkScanner::default(),
            scratch: String::new(),
            chars: Vec::new(),
        }
    }

    /// Consume bytes on their way from the user to the shell.
    pub fn on_input(&mut self, t: f64, bytes: &[u8], out: &mut Vec<Event>) {
        for &b in bytes {
            match b {
                b'\r' | b'\n' => {
                    let text = std::mem::take(&mut self.typed);
                    if self.typed_trustworthy && !text.trim().is_empty() {
                        self.last_submitted = Some(text.clone());
                        if !self.integration {
                            let id = self.take_id();
                            self.pending = Some(Pending { id, text });
                        }
                    }
                    self.typed_trustworthy = true;
                }
                0x7f | 0x08 => {
                    self.typed.pop();
                }
                0x15 => self.typed.clear(), // Ctrl-U: kill the line
                0x17 => {
                    // Ctrl-W: kill the previous word.
                    let trimmed = self.typed.trim_end();
                    let cut = trimmed.rfind(' ').map_or(0, |i| i);
                    self.typed.truncate(cut);
                }
                0x03 => {
                    // Ctrl-C: the line is discarded, and whatever was running
                    // is now finished.
                    self.typed.clear();
                    if self.integration {
                        self.close_open(t, Some(130), None, out);
                    }
                }
                0x01..=0x1a if !matches!(b, b'\t' | b'\n' | b'\r') => {
                    // Remaining control keys (arrows, function keys, Ctrl-R)
                    // can rewrite the line in ways we cannot model.
                    self.typed_trustworthy = false;
                }
                0x1b => {
                    // An escape sequence: almost always a cursor or history key.
                    // The output scanner is deliberately not fed from here; it
                    // has its own state machine over a different byte stream.
                    self.typed_trustworthy = false;
                }
                b if b < 0x20 => {}
                _ => self.push_typed(b),
            }
        }
    }

    /// Append one byte to the reconstructed input line, decoding UTF-8 so a
    /// split scalar does not become two replacement characters.
    fn push_typed(&mut self, b: u8) {
        loop {
            if b.is_ascii() && self.typed_utf8.is_idle() {
                self.typed.push(b as char);
                return;
            }
            match self.typed_utf8.push(b) {
                TextResult::Restart => self.typed.push(REPLACEMENT),
                TextResult::Pending => return,
                TextResult::Done(c) => {
                    self.typed.push(c);
                    return;
                }
            }
        }
    }

    /// Consume bytes on their way from the shell to the user.
    pub fn on_output(&mut self, t: f64, bytes: &[u8], out: &mut Vec<Event>) {
        let mut marks = Vec::new();
        self.scratch.clear();
        self.scanner.feed(bytes, &mut self.scratch, &mut marks);

        for mark in marks {
            self.apply_mark(t, mark, out);
        }

        // Feed the visible text through the line tracker to confirm echoes.
        // The characters are staged in a side buffer so that the loop can call
        // back into `self` while walking them.
        self.chars.clear();
        self.chars.extend(self.scratch.chars());
        for i in 0..self.chars.len() {
            match self.chars[i] {
                '\n' => {
                    self.break_line(t, out);
                    self.echo_line.clear();
                }
                '\t' => {
                    // Tabs align the echo, so compare on whitespace-folded text.
                    self.echo_line.push(' ');
                }
                other => self.echo_line.push(other),
            }
        }
    }

    /// A line break: if it ends the echo of what the user typed, that is our
    /// command boundary.
    fn break_line(&mut self, t: f64, out: &mut Vec<Event>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if normalize(&self.echo_line) == normalize(&pending.text) {
            self.open = Some(pending.id);
            out.push(Event::Command {
                t,
                id: pending.id,
                text: pending.text,
                source: CommandSource::Echo,
            });
        }
    }

    fn apply_mark(&mut self, t: f64, mark: Mark, out: &mut Vec<Event>) {
        match mark.kind {
            // `C` is the command-start signal in every shell `eye` integrates
            // with. bash and zsh emit `B` then `C` from a pre-execution hook;
            // fish emits `B` when it draws the prompt and `C` when the line is
            // accepted, which is the same instant. `B` on its own therefore
            // means "a prompt appeared", not "a command started", and only `C`
            // opens a command.
            b'C' => {
                if self.open.is_some() {
                    // fish emits both its own marks and the ones our hooks
                    // install. The first one to arrive is the command start;
                    // the rest are the same fact said twice.
                    return;
                }
                // Text comes from what we watched being typed, which is more
                // reliable than asking the shell.
                let text = self.last_submitted.take().unwrap_or_default();
                let id = self.take_id();
                self.open = Some(id);
                out.push(Event::Command { t, id, text, source: CommandSource::Integration });
            }
            b'D' => self.close_open(t, mark.status, None, out),
            _ => {}
        }
    }

    fn close_open(&mut self, t: f64, code: Option<i32>, signal: Option<i32>, out: &mut Vec<Event>) {
        if let Some(id) = self.open.take() {
            out.push(Event::Exit { t, id, code, signal });
        }
    }

    /// Emit an `Exit` for a command that never got a `D` mark, if one is open.
    pub fn finish(&mut self, t: f64, code: Option<i32>, out: &mut Vec<Event>) {
        self.close_open(t, code, None, out);
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The most recently submitted line, for tests and diagnostics.
    pub fn last_submitted(&self) -> Option<&str> {
        self.last_submitted.as_deref()
    }

    /// True once a command has been identified as running.
    pub fn has_open_command(&self) -> bool {
        self.open.is_some()
    }

    /// Forget everything; used when the shell starts.
    pub fn reset(&mut self) {
        self.typed.clear();
        self.last_submitted = None;
        self.pending = None;
        self.echo_line.clear();
        self.open = None;
        self.typed_trustworthy = true;
    }
}

/// Collapse whitespace so a tab-aligned echo still compares equal.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Encode raw input bytes back into an event, for callers that record input.
pub fn input_event(t: f64, bytes: &[u8]) -> Event {
    Event::Input { t, data: Bytes::from(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(tracker: &mut CommandTracker, out: &mut Vec<Event>, text: &str) {
        let mut events = Vec::new();
        tracker.on_output(0.5, text.as_bytes(), &mut events);
        out.extend(events);
    }

    fn submit(tracker: &mut CommandTracker, out: &mut Vec<Event>, text: &str) {
        tracker.on_input(0.4, text.as_bytes(), out);
        tracker.on_input(0.4, b"\r", out);
    }

    #[test]
    fn marks_are_split_out_of_the_text() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        scanner.feed(b"hi \x1b]133;D;7\x07there", &mut text, &mut marks);
        // The OSC string and its BEL are structure, not text: they are removed
        // rather than left to show up as noise in a command.
        assert_eq!(text, "hi there");
        assert_eq!(marks, vec![Mark { kind: b'D', status: Some(7) }]);
    }

    #[test]
    fn non_133_osc_is_ignored() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        scanner.feed(b"\x1b]0;a title\x07visible", &mut text, &mut marks);
        assert_eq!(text, "visible");
        assert!(marks.is_empty());
    }

    #[test]
    fn osc_marks_split_across_chunks_still_parse() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        scanner.feed(b"\x1b]133", &mut text, &mut marks);
        scanner.feed(b";D;3\x1b", &mut text, &mut marks);
        scanner.feed(b"\\\n", &mut text, &mut marks);
        assert_eq!(marks, vec![Mark { kind: b'D', status: Some(3) }]);
    }

    #[test]
    fn echo_reconstruction_finds_a_typed_command() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.10, b"cargo build", &mut out);
        tracker.on_input(0.20, b"\r", &mut out);
        assert!(out.is_empty(), "input alone does not create a command");

        // The terminal echoes the line, then the shell starts talking.
        echo(&mut tracker, &mut out, "cargo build");
        assert!(out.is_empty(), "the echo line has not ended yet");
        echo(&mut tracker, &mut out, "\r\n");
        assert_eq!(
            out,
            vec![Event::Command {
                t: 0.5,
                id: 0,
                text: "cargo build".into(),
                source: CommandSource::Echo,
            }]
        );
        assert!(tracker.has_open_command());
    }

    #[test]
    fn an_echo_that_does_not_match_produces_nothing() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"ls", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "something else\r\n");
        assert!(out.is_empty(), "a false command is worse than none");
        assert!(!tracker.has_open_command());
    }

    #[test]
    fn line_editing_is_reconstructed() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        // Two characters typed by mistake and rubbed out again.
        tracker.on_input(0.1, b"git statusxy", &mut out);
        tracker.on_input(0.1, &[0x7f, 0x7f], &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "git status\r\n");
        assert_eq!(
            out,
            vec![Event::Command {
                t: 0.5,
                id: 0,
                text: "git status".into(),
                source: CommandSource::Echo,
            }]
        );
    }

    #[test]
    fn ctrl_w_and_ctrl_u_clear_the_line() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"echo one two", &mut out);
        tracker.on_input(0.1, &[0x17], &mut out); // kill last word
        tracker.on_input(0.1, &[0x15], &mut out); // kill the line
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "\r\n");
        assert!(out.is_empty(), "an empty line is not a command");
    }

    #[test]
    fn history_recall_disables_tracking_rather_than_guessing() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"l", &mut out);
        tracker.on_input(0.1, b"\x1b[A", &mut out); // up arrow
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "ls -la\r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn integration_marks_give_commands_and_exit_codes() {
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        tracker.on_input(0.4, b"cargo build", &mut out);
        tracker.on_input(0.4, b"\r", &mut out);

        echo(&mut tracker, &mut out, "\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;C\x07cargo build\r\n");
        echo(&mut tracker, &mut out, "Compiling\r\n");
        echo(&mut tracker, &mut out, "done\r\n");

        let commands: Vec<_> = out.iter().filter(|e| matches!(e, Event::Command { .. })).collect();
        assert_eq!(commands.len(), 1, "{out:?}");
        match commands[0] {
            Event::Command { text, source, .. } => {
                assert_eq!(text, "cargo build");
                assert_eq!(*source, CommandSource::Integration);
            }
            other => panic!("{other:?}"),
        }

        let mut out = Vec::new();
        echo(&mut tracker, &mut out, "\x1b]133;D;101\x07\x1b]133;A\x07$ ");
        assert_eq!(
            out,
            vec![Event::Exit { t: 0.5, id: 0, code: Some(101), signal: None }]
        );
    }

    #[test]
    fn integration_ignores_echo_reconstruction_entirely() {
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        submit(&mut tracker, &mut out, "ls");
        echo(&mut tracker, &mut out, "ls\r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn ctrl_c_closes_an_open_integration_command() {
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"sleep 100", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "\x1b]133;B\x07\x1b]133;C\x07");
        assert!(tracker.has_open_command());
        tracker.on_input(0.9, &[0x03], &mut out);
        assert_eq!(out.last(), Some(&Event::Exit { t: 0.9, id: 0, code: Some(130), signal: None }));
    }

    #[test]
    fn a_backspace_in_the_echo_is_undone_by_the_terminal() {
        // Readline redrawing a line sends a backspace over the character it is
        // replacing. The reconstructed line is the one on screen afterwards,
        // which is what the user actually ran.
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"ls -l", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "ls -x\x08l\r\n");
        assert_eq!(
            out,
            vec![Event::Command { t: 0.5, id: 0, text: "ls -l".into(), source: CommandSource::Echo }]
        );
    }

    #[test]
    fn tab_alignment_does_not_break_comparison() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        submit(&mut tracker, &mut out, "ls   -la");
        echo(&mut tracker, &mut out, "ls\t-la\r\n");
        assert_eq!(
            out,
            vec![Event::Command {
                t: 0.5,
                id: 0,
                text: "ls   -la".into(),
                source: CommandSource::Echo,
            }]
        );
    }

    #[test]
    fn consecutive_commands_get_distinct_ids() {
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        for (i, cmd) in ["ls", "pwd"].iter().enumerate() {
            tracker.on_input(i as f64, cmd.as_bytes(), &mut out);
            tracker.on_input(i as f64, b"\r", &mut out);
            echo(&mut tracker, &mut out, "\x1b]133;B\x07\x1b]133;C\x07");
            echo(&mut tracker, &mut out, &format!("\x1b]133;D;{}\x07", i));
        }
        let ids: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                Event::Command { id, .. } | Event::Exit { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![0, 0, 1, 1]);
    }

    #[test]
    fn finish_closes_a_command_the_shell_never_reported_on() {
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"vim", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "\x1b]133;B\x07\x1b]133;C\x07");
        out.clear();
        tracker.finish(9.0, None, &mut out);
        assert_eq!(out, vec![Event::Exit { t: 9.0, id: 0, code: None, signal: None }]);
        // Idempotent: a second finish emits nothing.
        out.clear();
        tracker.finish(9.5, None, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_b_mark_alone_does_not_start_a_command() {
        // fish emits `B` when it draws a prompt. Treating that as "a command
        // started" would invent a command for every prompt.
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        echo(&mut tracker, &mut out, "\x1b]133;A\x07\x1b]133;B\x07$ ");
        assert!(out.is_empty(), "{out:?}");
        assert!(!tracker.has_open_command());
    }

    #[test]
    fn duplicate_c_marks_describe_one_command() {
        // fish reports OSC 133 natively *and* runs our hooks; both say `C`.
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"ls", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "\x1b]133;B\x07\x1b]133;C\x07");
        echo(&mut tracker, &mut out, "\x1b]133;C;cmdline_url=ls\x07");
        let commands: Vec<_> = out.iter().filter(|e| matches!(e, Event::Command { .. })).collect();
        assert_eq!(commands.len(), 1, "{out:?}");

        let mut out = Vec::new();
        echo(&mut tracker, &mut out, "\x1b]133;D;0\x07");
        echo(&mut tracker, &mut out, "\x1b]133;D;ls\x07");
        assert_eq!(out, vec![Event::Exit { t: 0.5, id: 0, code: Some(0), signal: None }]);
    }

    #[test]
    fn a_d_mark_with_nothing_open_is_discarded() {
        // Every integrated shell emits `D` once at start-up, before any
        // command has run.
        let mut tracker = CommandTracker::new(true);
        let mut out = Vec::new();
        echo(&mut tracker, &mut out, "\x1b]133;D;0\x07\x1b]133;A\x07");
        assert!(out.is_empty());
    }

    #[test]
    fn multibyte_text_survives_chunk_boundaries() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        // A redraw can cut "héllo → 🌍" anywhere, including mid-scalar.
        let chunks: [&[u8]; 5] = [b"h\xc3", b"\xa9llo ", b"\xe2\x86", b"\x92 \xf0\x9f", b"\x8c\x8d"];
        for chunk in chunks {
            scanner.feed(chunk, &mut text, &mut marks);
        }
        assert_eq!(text, "héllo → 🌍");
    }

    #[test]
    fn an_invalid_scalar_does_not_swallow_the_byte_after_it() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        // Lead byte 0xe2, then 'a' where a continuation was required.
        scanner.feed(b"\xe2a", &mut text, &mut marks);
        assert_eq!(text, "\u{fffd}a");
    }

    #[test]
    fn a_truncated_scalar_is_flushed_as_a_replacement() {
        let mut scanner = MarkScanner::default();
        let mut text = String::new();
        let mut marks = Vec::new();
        scanner.feed(b"ok\xf0\x9f", &mut text, &mut marks);
        assert_eq!(text, "ok", "incomplete input must not be emitted early");
        scanner.flush(&mut text);
        assert_eq!(text, "ok\u{fffd}");
    }

    #[test]
    fn typed_input_decodes_utf8_split_across_reads() {
        let mut tracker = CommandTracker::new(false);
        let mut out = Vec::new();
        tracker.on_input(0.1, b"echo \xf0\x9f", &mut out);
        tracker.on_input(0.1, b"\x8c\x8d", &mut out);
        tracker.on_input(0.1, b"\r", &mut out);
        echo(&mut tracker, &mut out, "echo 🌍\r\n");
        match out.first() {
            Some(Event::Command { text, .. }) => assert_eq!(text, "echo 🌍"),
            other => panic!("{other:?}"),
        }
    }
}
