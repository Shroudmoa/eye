//! Streaming reader for `.eye` files.
//!
//! The reader is deliberately tolerant: a recording may have been truncated by
//! a crash, a full disk, or a `kill -9`, and the worst case should be "the last
//! few events are missing", not "the file is unusable". Malformed lines are
//! reported as data rather than raised as fatal errors, and unknown event types
//! are preserved by the event model rather than skipped.

use crate::event::{Event, Meta, UnknownEventError};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Why reading an event failed.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A line that is not a valid event. Carries the raw text so callers can
    /// report or salvage it.
    #[error("line {line} is not a valid event: {source}")]
    Malformed {
        line: u64,
        raw: String,
        #[source]
        source: UnknownEventError,
    },
}

impl ReadError {
    /// 1-based line number, when the error has one.
    pub fn line(&self) -> Option<u64> {
        match self {
            ReadError::Io { .. } => None,
            ReadError::Malformed { line, .. } => Some(*line),
        }
    }
}

/// An open session file, positioned just after the header.
pub struct SessionReader {
    path: PathBuf,
    lines: io::Lines<BufReader<File>>,
    meta: Meta,
    line_no: u64,
}

impl SessionReader {
    /// Open a recording and read its header.
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        let file = File::open(path).map_err(|source| ReadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut lines = BufReader::with_capacity(64 * 1024, file).lines();

        let first = lines.next().ok_or_else(|| ReadError::Malformed {
            line: 1,
            raw: String::new(),
            source: UnknownEventError {
                tag: None,
                source: serde_json::Error::io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file is empty",
                )),
            },
        })?;
        let first = first.map_err(|source| ReadError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let meta = match crate::event::from_json_line(&first) {
            Ok(Event::Meta { meta }) => *meta,
            Ok(_) => {
                return Err(ReadError::Malformed {
                    line: 1,
                    raw: first,
                    source: UnknownEventError {
                        tag: Some("not-meta".into()),
                        source: serde_json::Error::io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "first record is not a meta header",
                        )),
                    },
                })
            }
            Err(source) => return Err(ReadError::Malformed { line: 1, raw: first, source }),
        };
        meta.check_compatible().map_err(|e| ReadError::Malformed {
            line: 1,
            raw: String::new(),
            source: UnknownEventError {
                tag: Some("meta".into()),
                source: serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, e)),
            },
        })?;

        Ok(SessionReader { path: path.to_path_buf(), lines, meta, line_no: 1 })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Next event, or `None` at end of file.
    ///
    /// Returns `Err` for I/O failures and malformed lines; callers that want to
    /// push through corruption should use [`Self::scan`].
    pub fn next_event(&mut self) -> Result<Option<Event>, ReadError> {
        loop {
            let line_no = self.line_no + 1;
            let line = match self.lines.next() {
                None => return Ok(None),
                Some(line) => line.map_err(|source| ReadError::Io {
                    path: self.path.clone(),
                    source,
                })?,
            };
            self.line_no = line_no;
            if line.trim().is_empty() {
                continue;
            }
            return match crate::event::from_json_line(&line) {
                Ok(event) => Ok(Some(event)),
                Err(source) => Err(ReadError::Malformed { line: line_no, raw: line, source }),
            };
        }
    }

    /// Iterate over every remaining event, including malformed ones as `Err`.
    pub fn events(&mut self) -> EventIter<'_> {
        EventIter { reader: self }
    }

    /// Read the whole session into memory. Fine for transcripts and exports;
    /// replay streams instead.
    pub fn read_all(&mut self) -> Result<Vec<Event>, ReadError> {
        let mut out = Vec::new();
        while let Some(event) = self.next_event()? {
            out.push(event);
        }
        Ok(out)
    }
}

impl Iterator for SessionReader {
    type Item = Result<Event, ReadError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_event().transpose()
    }
}

/// Iterator over a session's events; see [`SessionReader::events`].
pub struct EventIter<'a> {
    reader: &'a mut SessionReader,
}

impl Iterator for EventIter<'_> {
    type Item = Result<Event, ReadError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next_event().transpose()
    }
}

/// Everything `eye list` and `eye search` want to know about a file.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub meta: Meta,
    pub duration: f64,
    pub commands: u64,
    pub output_bytes: u64,
    pub events: u64,
    pub incomplete: bool,
    pub malformed: u64,
    /// Exit statuses of every command that reported one.
    pub exits: Vec<i32>,
}

impl SessionSummary {
    /// Scan a file without holding its events in memory.
    pub fn scan(path: &Path) -> Result<Self, ReadError> {
        let incomplete = crate::storage::is_part(path);
        let mut reader = SessionReader::open(path)?;
        let meta = reader.meta().clone();
        let mut duration = 0.0f64;
        let mut commands = 0;
        let mut output_bytes = 0;
        let mut events = 0;
        let mut malformed = 0;
        let mut exits = Vec::new();

        for item in reader.events() {
            match item {
                Ok(event) => {
                    events += 1;
                    if let Some(t) = event.t() {
                        duration = duration.max(t);
                    }
                    match event {
                        Event::Command { .. } => commands += 1,
                        Event::Exit { code: Some(c), .. } => exits.push(c),
                        Event::Output { data, .. } => output_bytes += data.len() as u64,
                        _ => {}
                    }
                }
                Err(_) => malformed += 1,
            }
        }

        Ok(SessionSummary {
            path: path.to_path_buf(),
            meta,
            duration,
            commands,
            output_bytes,
            events,
            incomplete,
            malformed,
            exits,
        })
    }

    /// Did the session end with a failing command?
    pub fn has_failure(&self) -> bool {
        self.exits.iter().any(|c| *c != 0)
    }
}

/// Read an entire file into events, skipping (and counting) malformed lines.
pub fn read_all_tolerant(path: &Path) -> Result<(Vec<Event>, u64), ReadError> {
    let mut reader = SessionReader::open(path)?;
    let mut events = Vec::new();
    let mut malformed = 0;
    for item in reader.events() {
        match item {
            Ok(event) => events.push(event),
            Err(ReadError::Io { .. }) => return Err(ReadError::Io {
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::Other, "read failed mid-stream"),
            }),
            Err(_) => malformed += 1,
        }
    }
    Ok((events, malformed))
}
