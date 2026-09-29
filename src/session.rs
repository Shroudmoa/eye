//! The session layer: a recording read back as something you can ask questions
//! of.
//!
//! Storage knows about bytes and lines; the event model knows about a single
//! event. This module sits between them and hands the rest of `eye` one
//! `Session` value with the questions already answered:
//!
//! * `timeline` — output and resize events merged into recorded order, which is
//!   the order replay has to draw them in.
//! * `commands` / `exits` — what was typed and how it ended.
//! * `duration` — how long the session ran.
//!
//! Output payloads are kept in one contiguous buffer with offsets into it,
//! rather than one `Vec<u8>` per chunk: a three-hour session is tens of
//! thousands of chunks, and 48 bytes of allocator overhead each is the
//! difference between "opens instantly" and "opens eventually".
//!
//! Loading is a single pass and is tolerant: a recording truncated by a crash
//! yields everything up to the damage, with a count of what was lost.

use crate::event::{CommandSource, Event, Meta, Size, Stream};
use crate::storage::{ReadError, SessionReader};
use crate::term::Screen;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// One drawable thing that happened, in recorded order.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// Bytes the session produced, as a slice of [`Session::output`].
    Output { t: f64, range: Range<usize>, stream: Stream },
    /// Terminal geometry changed.
    Resize { t: f64, size: Size },
}

impl Item {
    pub fn t(&self) -> f64 {
        match self {
            Item::Output { t, .. } | Item::Resize { t, .. } => *t,
        }
    }

    /// The output bytes, given the session's buffer.
    pub fn bytes<'a>(&self, output: &'a [u8]) -> &'a [u8] {
        match self {
            Item::Output { range, .. } => &output[range.clone()],
            Item::Resize { .. } => &[],
        }
    }
}

/// A command line that was submitted.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub id: u64,
    pub t: f64,
    pub text: String,
    pub source: CommandSource,
    /// Verdict, once known: filled in from the matching [`Exit`].
    pub exit: Option<Exit>,
}

/// How a command finished.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exit {
    pub id: u64,
    pub t: f64,
    /// Exit status for a normal exit.
    pub code: Option<i32>,
    /// Signal number when the command was killed.
    pub signal: Option<i32>,
}

impl Exit {
    /// True when this exit is a failure, by the usual shell convention.
    pub fn is_failure(&self) -> bool {
        match (self.code, self.signal) {
            (Some(code), _) => code != 0,
            (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    /// `exit 1`, `signal:9 (KILL)`, or `unknown` when neither is known.
    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(signo)) => format!("signal:{} ({})", signo, crate::tty::short_signal_name(signo)),
            (None, None) => "unknown".into(),
        }
    }
}

/// A signal `eye` observed or forwarded.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalRecord {
    pub t: f64,
    pub name: String,
    pub number: i32,
}

/// A named point of interest.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkRecord {
    pub t: f64,
    pub label: String,
    pub note: Option<String>,
}

/// Why a recording stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct End {
    pub t: f64,
    pub reason: String,
    pub code: Option<i32>,
}

/// A whole recording, parsed once.
#[derive(Clone, Debug)]
pub struct Session {
    pub path: PathBuf,
    pub meta: Meta,
    /// Every event after the header, including unknown ones, in file order.
    pub events: Vec<Event>,
    /// Output payloads concatenated in recorded order.
    output: Vec<u8>,
    /// Output and resize events merged, in recorded order.
    pub timeline: Vec<Item>,
    pub commands: Vec<Command>,
    /// Exits that never matched a command, in recorded order.
    pub orphan_exits: Vec<Exit>,
    pub signals: Vec<SignalRecord>,
    pub marks: Vec<MarkRecord>,
    pub end: Option<End>,
    /// The geometry the session opened at, if it recorded a `session_start`.
    /// Preferred over the header's copy: the header is written before the PTY
    /// is measured, so the event is the one that matches the byte stream.
    start_size: Option<Size>,
    pub duration: f64,
    /// Lines that could not be parsed.
    pub malformed: u64,
    /// Events whose type this build does not implement (still preserved).
    pub unknown: u64,
}

impl Session {
    /// Read a recording from disk.
    pub fn load(path: &Path) -> Result<Self, ReadError> {
        let mut reader = SessionReader::open(path)?;
        let meta = reader.meta().clone();

        let mut session = Session {
            path: path.to_path_buf(),
            meta,
            events: Vec::new(),
            output: Vec::new(),
            timeline: Vec::new(),
            commands: Vec::new(),
            orphan_exits: Vec::new(),
            signals: Vec::new(),
            marks: Vec::new(),
            end: None,
            start_size: None,
            duration: 0.0,
            malformed: 0,
            unknown: 0,
        };

        for item in reader.events() {
            let event = match item {
                Ok(event) => event,
                Err(ReadError::Io { .. }) => {
                    return Err(ReadError::Io {
                        path: path.to_path_buf(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "read failed while loading the session",
                        ),
                    })
                }
                Err(_) => {
                    // A corrupt line is a fact about the file, not a reason to
                    // throw away the rest of the session.
                    session.malformed += 1;
                    continue;
                }
            };
            if event.is_unknown() {
                session.unknown += 1;
            }
            if let Some(t) = event.t() {
                session.duration = session.duration.max(t);
            }
            session.absorb(&event);
            session.events.push(event);
        }

        session.attach_exits();
        Ok(session)
    }

    /// Fold one event into the derived views.
    fn absorb(&mut self, event: &Event) {
        match event {
            Event::Output { t, data, stream } => {
                let start = self.output.len();
                self.output.extend_from_slice(data.as_slice());
                self.timeline.push(Item::Output { t: *t, range: start..self.output.len(), stream: *stream });
            }
            Event::Resize { t, size } => {
                self.timeline.push(Item::Resize { t: *t, size: *size });
            }
            Event::Command { t, id, text, source } => {
                self.commands.push(Command {
                    id: *id,
                    t: *t,
                    text: text.clone(),
                    source: *source,
                    exit: None,
                });
            }
            Event::Exit { t, id, code, signal } => {
                let exit = Exit { id: *id, t: *t, code: *code, signal: *signal };
                match self.commands.iter_mut().find(|c| c.id == *id) {
                    Some(command) => command.exit = Some(exit),
                    None => self.orphan_exits.push(exit),
                }
            }
            Event::Signal { t, name, number } => {
                self.signals.push(SignalRecord { t: *t, name: name.clone(), number: *number });
            }
            Event::Mark { t, label, note } => {
                self.marks.push(MarkRecord { t: *t, label: label.clone(), note: note.clone() });
            }
            Event::SessionEnd { t, reason, code } => {
                self.end = Some(End { t: *t, reason: reason.clone(), code: *code });
            }
            Event::SessionStart { size, .. } => {
                self.start_size = Some(*size);
            }
            Event::Meta { .. }
            | Event::Input { .. }
            | Event::Mouse { .. }
            | Event::Unknown(_) => {}
        }
    }

    /// Give every command the verdict that arrived later in the file.
    fn attach_exits(&mut self) {
        // Commands are pushed in order and exits reference them by id, so a
        // single reverse pass is enough and no lookup structure is needed.
        let mut exits: Vec<(u64, Exit)> = Vec::new();
        for event in &self.events {
            if let Event::Exit { t, id, code, signal } = event {
                exits.push((*id, Exit { id: *id, t: *t, code: *code, signal: *signal }));
            }
        }
        for command in &mut self.commands {
            command.exit = exits.iter().find(|(id, _)| *id == command.id).map(|(_, e)| *e);
        }
    }

    /// Output payload bytes, concatenated.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Total number of output bytes.
    pub fn output_bytes(&self) -> u64 {
        self.output.len() as u64
    }

    /// The recorded terminal size.
    ///
    /// The `session_start` event is preferred over the header's copy: the
    /// header is written before the PTY reports its size, so the event is the
    /// one that matches the byte stream.
    pub fn recorded_size(&self) -> Size {
        self.start_size.unwrap_or(self.meta.terminal.size)
    }

    /// Number of commands that finished unsuccessfully.
    pub fn failures(&self) -> usize {
        self.commands.iter().filter(|c| c.exit.is_some_and(|e| e.is_failure())).count()
    }

    /// Commands still running when the recording stopped.
    pub fn unfinished(&self) -> usize {
        self.commands.iter().filter(|c| c.exit.is_none()).count()
    }

    /// The `Item`s due at or before `t`.
    ///
    /// Replay seeks by asking for everything up to a timestamp; doing it as a
    /// partition point keeps a seek O(log n) to *find* and O(n) to redraw,
    /// which is the honest cost of rebuilding a screen.
    pub fn items_upto(&self, t: f64) -> usize {
        self.timeline.partition_point(|item| item.t() <= t)
    }

    /// The output bytes recorded by timeline items `from..to`, concatenated.
    ///
    /// Exporters use this to attribute output to the command it followed
    /// without re-walking the event list. Resize items contribute no bytes.
    pub fn output_between(&self, from: usize, to: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.timeline[from.min(self.timeline.len())..to.min(self.timeline.len())] {
            if let Item::Output { range, .. } = item {
                out.extend_from_slice(&self.output[range.clone()]);
            }
        }
        out
    }

    /// Feed every recorded byte through a screen model.
    ///
    /// Used by HTML export, by `--tail`-style inspection and by anything else
    /// that needs "what did the terminal actually look like". `size` is the
    /// geometry to render at; pass `None` for the recorded size.
    pub fn screen(&self, size: Option<Size>) -> Screen {
        let mut screen = Screen::new(size.unwrap_or_else(|| self.recorded_size()));
        for item in &self.timeline {
            match item {
                Item::Output { range, .. } => screen.feed(&self.output[range.clone()]),
                Item::Resize { size, .. } => screen.resize(*size),
            }
        }
        screen
    }

    /// The text the terminal showed at `t`, without escape codes.
    pub fn screen_text_at(&self, t: f64, size: Option<Size>) -> Vec<String> {
        let mut screen = Screen::new(size.unwrap_or_else(|| self.recorded_size()));
        for item in &self.timeline {
            if item.t() > t {
                break;
            }
            match item {
                Item::Output { range, .. } => screen.feed(&self.output[range.clone()]),
                Item::Resize { size, .. } => screen.resize(*size),
            }
        }
        screen.text_rows()
    }

    /// Was this recording left unfinished (no `session_end`)?
    pub fn is_incomplete(&self) -> bool {
        self.end.is_none() || crate::storage::is_part(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Bytes;
    use crate::testsupport;

    fn write_session(name: &str, events: &[Event]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eye-s-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.eye");
        let mut w = crate::storage::SessionWriter::create(&path, testsupport::meta()).unwrap();
        for e in events {
            w.push(e).unwrap();
        }
        w.finish("shell_exit", Some(0)).unwrap();
        path
    }

    fn output(t: f64, text: &str) -> Event {
        Event::Output { t, data: Bytes::from(text.as_bytes().to_vec()), stream: Stream::Tty }
    }

    #[test]
    fn timeline_merges_output_and_resize_in_recorded_order() {
        let path = write_session(
            "merge",
            &[
                output(0.0, "one"),
                Event::Resize { t: 0.5, size: Size::new(100, 40) },
                output(1.0, "two"),
            ],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(
            s.timeline,
            vec![
                Item::Output { t: 0.0, range: 0..3, stream: Stream::Tty },
                Item::Resize { t: 0.5, size: Size::new(100, 40) },
                Item::Output { t: 1.0, range: 3..6, stream: Stream::Tty },
            ]
        );
        // Slice offsets must survive the shared buffer.
        let first = s.timeline[0].bytes(s.output());
        assert_eq!(first, b"one");
        assert_eq!(s.output(), b"onetwo");
    }

    #[test]
    fn duration_is_the_last_timestamp() {
        let path = write_session(
            "duration",
            &[
                output(0.0, "a"),
                Event::Resize { t: 3.25, size: Size::new(80, 24) },
                output(9.5, "b"),
            ],
        );
        assert_eq!(Session::load(&path).unwrap().duration, 9.5);
    }

    #[test]
    fn commands_are_paired_with_their_exit_even_though_it_comes_later() {
        let path = write_session(
            "commands",
            &[
                Event::Command { t: 1.0, id: 1, text: "ls".into(), source: CommandSource::Integration },
                Event::Command { t: 2.0, id: 2, text: "false".into(), source: CommandSource::Integration },
                Event::Exit { t: 1.1, id: 1, code: Some(0), signal: None },
                Event::Exit { t: 2.1, id: 2, code: Some(1), signal: None },
            ],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(s.commands.len(), 2);
        assert_eq!(s.commands[0].exit.unwrap().describe(), "exit 0");
        assert_eq!(s.commands[1].exit.unwrap().describe(), "exit 1");
        assert!(!s.commands[0].exit.unwrap().is_failure());
        assert!(s.commands[1].exit.unwrap().is_failure());
        assert_eq!(s.failures(), 1);
        assert_eq!(s.unfinished(), 0);
    }

    #[test]
    fn a_command_that_never_reports_is_unfinished_not_lost() {
        let path = write_session(
            "unfinished",
            &[Event::Command { t: 1.0, id: 7, text: "sleep 100".into(), source: CommandSource::Echo }],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(s.unfinished(), 1);
        assert!(s.commands[0].exit.is_none());
    }

    #[test]
    fn a_signal_kill_is_a_failure_with_a_readable_description() {
        let path = write_session(
            "signalkill",
            &[
                Event::Command { t: 0.0, id: 1, text: "yes".into(), source: CommandSource::Integration },
                Event::Exit { t: 1.0, id: 1, code: None, signal: Some(libc::SIGKILL) },
            ],
        );
        let s = Session::load(&path).unwrap();
        let exit = s.commands[0].exit.unwrap();
        assert!(exit.is_failure());
        assert_eq!(exit.describe(), "signal:9 (KILL)");
    }

    #[test]
    fn exits_without_a_command_are_kept_rather_than_dropped() {
        let path = write_session(
            "orphan",
            &[Event::Exit { t: 4.0, id: 99, code: Some(3), signal: None }],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(s.orphan_exits.len(), 1);
        assert_eq!(s.orphan_exits[0].id, 99);
    }

    #[test]
    fn unknown_events_are_preserved_and_counted() {
        let path = write_session(
            "unknown",
            &[
                output(0.0, "x"),
                Event::unknown(r#"{"type":"telemetry","t":1.0,"widgets":42}"#).unwrap(),
                Event::unknown(r#"{"type":"telemetry","t":2.0,"widgets":43}"#).unwrap(),
            ],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(s.unknown, 2);
        // Two unknowns plus the output chunk, plus the trailer `finish` wrote.
        assert_eq!(s.events.len(), 4);
        // Re-serialising must give the bytes back untouched.
        let raw: Vec<&str> = s.events.iter().filter_map(|e| e.raw()).collect();
        assert_eq!(raw, vec![
            r#"{"type":"telemetry","t":1.0,"widgets":42}"#,
            r#"{"type":"telemetry","t":2.0,"widgets":43}"#,
        ]);
    }

    #[test]
    fn a_corrupt_line_costs_one_line_and_not_the_session() {
        let dir = std::env::temp_dir().join(format!("eye-s-{}-corrupt", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.eye");
        let mut w = crate::storage::SessionWriter::create(&path, testsupport::meta()).unwrap();
        w.push(&output(0.0, "before")).unwrap();
        w.finish("shell_exit", Some(0)).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json at all}\n");
        text.push_str(&crate::event::to_json_line(&output(2.0, "after")).unwrap());
        text.push('\n');
        std::fs::write(&path, text).unwrap();

        let s = Session::load(&path).unwrap();
        assert_eq!(s.malformed, 1);
        assert_eq!(s.timeline.len(), 2);
        assert_eq!(s.duration, 2.0);
    }

    #[test]
    fn items_upto_finds_the_cut_point_without_walking_the_timeline() {
        let path = write_session(
            "upto",
            &[output(0.0, "a"), output(1.0, "b"), output(2.0, "c")],
        );
        let s = Session::load(&path).unwrap();
        assert_eq!(s.items_upto(-1.0), 0);
        assert_eq!(s.items_upto(0.0), 1);
        assert_eq!(s.items_upto(1.5), 2);
        assert_eq!(s.items_upto(99.0), 3);
    }

    #[test]
    fn the_screen_model_sees_what_the_terminal_would_have_shown() {
        let path = write_session(
            "screen",
            &[
                output(0.0, "hello"),
                output(1.0, "\x1b[2J\x1b[Hworld"),
                Event::Resize { t: 2.0, size: Size::new(20, 5) },
            ],
        );
        let s = Session::load(&path).unwrap();
        let text = s.screen_text_at(0.5, Some(Size::new(20, 5)));
        assert_eq!(text[0], "hello");
        assert_eq!(s.screen_text_at(1.5, Some(Size::new(20, 5)))[0], "world");
        assert_eq!(s.screen(Some(Size::new(20, 5))).size(), Size::new(20, 5));
    }
}
