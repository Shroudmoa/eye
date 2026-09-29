//! `eye` — a terminal session recorder that stores sessions as structured,
//! replayable data.
//!
//! The recording is the source of truth; the terminal is just one way to look
//! at it. See `docs/architecture.md` for why the layers are shaped this way.

pub mod cli;
pub mod event;
pub mod pty;
pub mod recorder;
pub mod replay;
pub mod report;
pub mod session;
pub mod storage;
pub mod term;
pub mod tty;
pub mod util;

#[cfg(test)]
pub(crate) mod testsupport;
