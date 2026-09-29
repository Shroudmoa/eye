//! The event model.
//!
//! Design notes (see `docs/format.md` for the full contract):
//!
//! * `Event` is an *open* enum. The `Unknown` variant keeps the exact bytes of
//!   any event type this build does not understand, so forward compatibility is
//!   lossless rather than best-effort: a recording made by a future `eye` can
//!   be read, replayed and re-exported unchanged by an older binary.
//! * `Bytes` is base64 on the wire. PTY traffic is arbitrary bytes (invalid
//!   UTF-8 happens), and byte fidelity is the whole point of a recording.
//! * Timestamps are `f64` seconds relative to session start, measured with a
//!   monotonic clock, so they survive wall-clock adjustment.
//! * Evolution policy: additive changes (new optional fields, new tags, new
//!   event types) are always readable. Anything that would break an existing
//!   reader requires a `version` major bump, and readers refuse those.

use serde::{Deserialize, Serialize};
use std::fmt;

/// On-disk format version. Bump `MAJOR` for incompatible changes, `MINOR` for
/// additive ones. A reader must refuse a file with a newer *major* version.
pub const FORMAT_VERSION: u16 = 1;

/// The magic string identifying an `eye` session file.
pub const FORMAT_NAME: &str = "eye-session";

/// Defines `Event` (the open, user-facing enum) and `KnownEvent` (the closed
/// enum that `serde` derives against) from a single variant list, so the two
/// can never drift apart.
macro_rules! define_events {
    ($( $(#[$vmeta:meta])* $variant:ident { $( $(#[$fmeta:meta])* $field:ident : $fty:ty ),* $(,)? } ),* $(,)?) => {
        /// Closed set of events this build understands. Derived serde impls.
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub(crate) enum KnownEvent {
            $(
                $(#[$vmeta])*
                $variant { $( $(#[$fmeta])* $field: $fty ),* }
            ,)*
        }

        /// A recorded event. Unknown event types are preserved verbatim.
        #[derive(Clone, Debug)]
        pub enum Event {
            $(
                $(#[$vmeta])*
                $variant { $( $(#[$fmeta])* $field: $fty ),* }
            ,)*
            /// An event type this build does not implement.
            Unknown(Box<serde_json::value::RawValue>),
        }

        impl From<KnownEvent> for Event {
            fn from(k: KnownEvent) -> Event {
                match k {
                    $(
                        KnownEvent::$variant { $( $field ),* } => Event::$variant { $( $field ),* },
                    )*
                }
            }
        }
        impl From<Event> for KnownEvent {
            fn from(e: Event) -> KnownEvent {
                match e {
                    $(
                        Event::$variant { $( $field ),* } => KnownEvent::$variant { $( $field ),* },
                    )*
                    Event::Unknown(_) => unreachable!("unknown events have no known representation"),
                }
            }
        }

        // `RawValue` has no `PartialEq`, so compare its bytes directly. That
        // also means "unchanged" is decided by exact bytes rather than by
        // re-encoded JSON, which is what a lossless round-trip test asserts.
        impl PartialEq for Event {
            fn eq(&self, other: &Event) -> bool {
                match (self, other) {
                    (Event::Unknown(a), Event::Unknown(b)) => a.get() == b.get(),
                    (Event::Unknown(_), _) | (_, Event::Unknown(_)) => false,
                    (a, b) => a.as_known() == b.as_known(),
                }
            }
        }
    };
}

define_events! {
    /// First line of the file. Untimed: it describes the session, not part of it.
    Meta { meta: Box<Meta> },

    /// The shell's PTY is open and the session clock has started.
    SessionStart { t: f64, size: Size },

    /// Bytes the session produced. `stream` is `tty` unless the session was
    /// recorded with `--split-stderr`.
    Output { t: f64, data: Bytes, stream: Stream },

    /// Bytes the user sent to the session.
    Input { t: f64, data: Bytes },

    /// Terminal geometry changed.
    Resize { t: f64, size: Size },

    /// A command line was submitted. `source` is `echo` unless an integrated
    /// shell emitted OSC 133 marks, which is authoritative.
    Command { t: f64, id: u64, text: String, source: CommandSource },

    /// A command finished. `code` is set on a normal exit, `signal` otherwise.
    Exit { t: f64, id: u64, code: Option<i32>, signal: Option<i32> },

    /// A named point of interest.
    Mark { t: f64, label: String, note: Option<String> },

    /// A signal `eye` observed or forwarded to the session.
    Signal { t: f64, name: String, number: i32 },

    /// Mouse input, captured when mouse reporting was enabled.
    Mouse { t: f64, data: Bytes },

    /// Written when a session finishes cleanly. `reason` is `shell_exit`,
    /// `signal`, `error`, or `eof`.
    SessionEnd { t: f64, reason: String, code: Option<i32> },
}

impl Event {
    /// Event timestamp in seconds from session start. `Meta` is not timed.
    pub fn t(&self) -> Option<f64> {
        match self {
            Event::Meta { .. } => None,
            Event::SessionStart { t, .. }
            | Event::Output { t, .. }
            | Event::Input { t, .. }
            | Event::Resize { t, .. }
            | Event::Command { t, .. }
            | Event::Exit { t, .. }
            | Event::Mark { t, .. }
            | Event::Signal { t, .. }
            | Event::Mouse { t, .. }
            | Event::SessionEnd { t, .. } => Some(*t),
            Event::Unknown(_) => None,
        }
    }

    /// The stable `type` discriminator as it appears on disk.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Meta { .. } => "meta",
            Event::SessionStart { .. } => "session_start",
            Event::Output { .. } => "output",
            Event::Input { .. } => "input",
            Event::Resize { .. } => "resize",
            Event::Command { .. } => "command",
            Event::Exit { .. } => "exit",
            Event::Mark { .. } => "mark",
            Event::Signal { .. } => "signal",
            Event::Mouse { .. } => "mouse",
            Event::SessionEnd { .. } => "session_end",
            Event::Unknown(_) => "unknown",
        }
    }

    /// The closed-enum view of this event, if it is one we understand.
    pub(crate) fn as_known(&self) -> Option<KnownEvent> {
        match self {
            Event::Unknown(_) => None,
            other => Some(other.clone().into()),
        }
    }

    /// True for any event that is not part of the known set.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Event::Unknown(_))
    }

    /// Raw JSON of an unknown event, for byte-exact round-tripping.
    pub fn raw(&self) -> Option<&str> {
        match self {
            Event::Unknown(raw) => Some(raw.get()),
            _ => None,
        }
    }

    /// Rebuild an `Unknown` from raw JSON.
    pub fn unknown(raw: &str) -> Result<Event, serde_json::Error> {
        let value: Box<serde_json::value::RawValue> = serde_json::from_str(raw)?;
        Ok(Event::Unknown(value))
    }
}

/// A byte payload that serialises as base64.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn new(v: Vec<u8>) -> Self {
        Bytes(v)
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}
impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Bytes(v.to_vec())
    }
}
impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes(len={})", self.0.len())
    }
}

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use base64::Engine as _;
        s.serialize_str(&B64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use base64::Engine as _;
        let s = String::deserialize(d)?;
        B64.decode(s.as_bytes())
            .map(Bytes)
            .map_err(|e| serde::de::Error::custom(format!("invalid base64 payload: {e}")))
    }
}

/// Which side of the session an [`Event::Output`] payload came from.
///
/// A PTY merges stdout and stderr by construction — that is what makes an
/// interactive program behave correctly, and it is why `eye` defaults to
/// [`Stream::Tty`]. With `--split-stderr` the child's stderr is routed to a
/// dedicated pipe and recorded as [`Stream::Pipe`], which yields exact stderr
/// attribution at the cost of best-effort interleaving with the TTY stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// The controlling PTY: stdout *and* stderr as the terminal would see them.
    #[default]
    Tty,
    /// A dedicated pipe carrying the child's stderr.
    Pipe,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Tty => "tty",
            Stream::Pipe => "pipe",
        }
    }
}

/// How a command event was derived, so consumers can weight confidence:
/// shell integration is authoritative, echo reconstruction is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    /// Reconstructed from input keystrokes matched against terminal echo.
    #[default]
    Echo,
    /// OSC 133 marks emitted by an integrated shell. Authoritative.
    Integration,
}

impl CommandSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandSource::Echo => "echo",
            CommandSource::Integration => "integration",
        }
    }
}

/// Terminal geometry in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub fn new(cols: u16, rows: u16) -> Self {
        Size { cols, rows }
    }
    /// A sane size for a terminal that has not been sized yet.
    pub fn fallback() -> Self {
        Size { cols: 80, rows: 24 }
    }
}

/// Identifies the shell that was spawned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInfo {
    /// Resolved executable, e.g. `/usr/bin/fish`.
    pub path: String,
    /// Basename, e.g. `fish`.
    pub name: String,
    /// Argument vector used to start it.
    pub args: Vec<String>,
    /// Which command-detection strategy ended up being usable.
    pub integration: String,
}

/// Terminal description captured at session start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub term: String,
    pub colorterm: Option<String>,
    pub size: Size,
}

/// Header record. Always the first line of an `.eye` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Meta {
    /// Always [`FORMAT_NAME`].
    pub format: String,
    pub version: u16,
    /// Binary that produced the file, e.g. `eye 0.1.0`.
    pub tool: String,
    pub tool_version: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// Seconds since the Unix epoch, for tooling that prefers numbers.
    pub created_at_unix: f64,
    /// Unique session id, `<epoch-millis>-<pid>`.
    pub id: String,
    pub title: Option<String>,
    pub hostname: Option<String>,
    pub cwd: Option<String>,
    pub shell: ShellInfo,
    pub terminal: TerminalInfo,
    /// Selected environment, kept small and provenance-oriented.
    pub env: Vec<(String, String)>,
}

impl Meta {
    /// Reject files we were not written to understand.
    pub fn check_compatible(&self) -> Result<(), UnsupportedVersion> {
        if self.version > FORMAT_VERSION {
            Err(UnsupportedVersion {
                found: self.version,
                supported: FORMAT_VERSION,
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "this recording uses .eye format v{found}, but this build of eye only understands up to v{supported}; \
     upgrade eye to view it"
)]
pub struct UnsupportedVersion {
    pub found: u16,
    pub supported: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    pub(crate) fn sample_meta() -> Meta {
        Meta {
            format: FORMAT_NAME.into(),
            version: FORMAT_VERSION,
            tool: "eye".into(),
            tool_version: "0.1.0".into(),
            created_at: "2026-09-29T00:00:00Z".into(),
            created_at_unix: 1_700_000_000.0,
            id: "id".into(),
            title: None,
            hostname: None,
            cwd: None,
            shell: ShellInfo {
                path: "/bin/bash".into(),
                name: "bash".into(),
                args: vec!["-i".into()],
                integration: "echo".into(),
            },
            terminal: TerminalInfo {
                term: "xterm-256color".into(),
                colorterm: None,
                size: Size::new(80, 24),
            },
            env: vec![],
        }
    }

    #[test]
    fn known_event_converts_both_ways() {
        let a = Event::Output { t: 1.0, data: Bytes::new(vec![1, 2]), stream: Stream::Pipe };
        let k: KnownEvent = a.clone().into();
        assert_eq!(Event::from(k), a);
    }
}
