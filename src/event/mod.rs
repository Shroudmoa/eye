//! The event model: the source of truth for everything `eye` records.
//!
//! See `docs/format.md` for the on-disk contract. The rules that matter:
//!
//! * Every event carries a monotonic `t` (seconds since session start).
//! * `type` is the discriminant; unknown types are preserved verbatim so that
//!   a recording written by a future `eye` can still be read (and re-written
//!   unchanged) by an older binary.
//! * `version` lives once, in the `Meta` record at the top of the file.

pub mod model;
pub mod serialize;

pub use model::{
    Bytes, CommandSource, Event, Meta, ShellInfo, Size, Stream, TerminalInfo,
    FORMAT_NAME, FORMAT_VERSION,
};
pub use serialize::{from_json_line, to_json_line, UnknownEventError, KNOWN_TAGS};
