//! The non-terminal views: inspect, list, search and export.
//!
//! These are all the same question asked four ways — "what is in this
//! recording?" — and they all read from one [`Session`]. Keeping them together
//! means the rules about what counts as a failure, how output becomes text,
//! and where an answer is honest versus a guess are decided once.
//!
//! A note on stderr: a pty merges stdout and stderr by construction, because
//! that is what makes an interactive program behave correctly. So without
//! `Stream::Pipe` output there is *no* way to know which bytes were stderr.
//! Anything that claims to know is guessing from how error text is spelled,
//! and [`looks_like_error`] is where that guess lives — documented as a guess
//! so nobody has to discover it the hard way.

use crate::cli::{ExportFormat, InspectArgs, SearchArgs};
use crate::event::{CommandSource, Event, Size};
use crate::session::{Exit, Item, Session};
use crate::storage::{self, SessionSummary};
use crate::term::{Attrs, Color, Screen};
use crate::util::text::{first_line, strip_ansi, truncate_chars};
use crate::util::time::{format_duration, format_offset, format_rfc3339};
use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// How wide the `text` column gets before it is elided.
const TEXT_WIDTH: usize = 100;

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

/// What a timeline row is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    SessionStart,
    Command,
    Exit,
    Resize,
    Signal,
    Mark,
    Error,
    SessionEnd,
    Output,
    Input,
}

impl RowKind {
    /// The label in the `kind` column.
    pub fn label(self) -> &'static str {
        match self {
            RowKind::SessionStart => "start",
            RowKind::Command => "cmd",
            RowKind::Exit => "exit",
            RowKind::Resize => "size",
            RowKind::Signal => "signal",
            RowKind::Mark => "mark",
            RowKind::Error => "error",
            RowKind::SessionEnd => "end",
            RowKind::Output => "out",
            RowKind::Input => "in",
        }
    }
}

/// One line of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Seconds from session start.
    pub t: f64,
    pub kind: RowKind,
    /// Already stripped of escape codes and truncated to [`TEXT_WIDTH`].
    pub text: String,
    /// The command this row belongs to, when it is meaningful. Used by
    /// `--screen` and by the `out` rows in `--timeline` mode.
    pub command: Option<u64>,
}

/// Build the timeline for `eye inspect`.
///
/// Two shapes: the summary view (commands, their exits, resizes, signals, the
/// end) and the raw view (`--timeline`, which includes every output and input
/// event). Both end up sorted by timestamp, because events of different kinds
/// interleave and a reader wants one time-ordered stream.
pub fn inspect_rows(session: &Session, args: &InspectArgs) -> Vec<Row> {
    if args.timeline {
        return raw_rows(session);
    }
    summary_rows(session)
}

fn summary_rows(session: &Session) -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(Row {
        t: 0.0,
        kind: RowKind::SessionStart,
        text: format!(
            "{} {} {}x{}",
            session.meta.shell.name,
            session.meta.terminal.term,
            session.recorded_size().cols,
            session.recorded_size().rows
        ),
        command: None,
    });

    for command in &session.commands {
        let failed = command.exit.is_some_and(|e| e.is_failure());
        rows.push(Row {
            t: command.t,
            // A command that failed is reported once, as an error. Listing it
            // under both `cmd` and `error` would double-count the one thing the
            // reader is scanning for.
            kind: if failed { RowKind::Error } else { RowKind::Command },
            text: flatten(&command.text, TEXT_WIDTH),
            command: Some(command.id),
        });
        if let Some(exit) = command.exit {
            rows.push(Row { t: exit.t, kind: RowKind::Exit, text: describe_exit(&exit), command: Some(command.id) });
        }
    }

    for item in &session.timeline {
        if let Item::Resize { t, size } = item {
            rows.push(Row { t: *t, kind: RowKind::Resize, text: format!("{}x{}", size.cols, size.rows), command: None });
        }
    }
    for signal in &session.signals {
        rows.push(Row { t: signal.t, kind: RowKind::Signal, text: signal.name.clone(), command: None });
    }
    for mark in &session.marks {
        rows.push(Row { t: mark.t, kind: RowKind::Mark, text: mark.label.clone(), command: None });
    }
    // Exits with no matching command: the recording is damaged, or the command
    // happened before recording started. Either way it is worth showing rather
    // than silently dropping.
    for exit in &session.orphan_exits {
        rows.push(Row { t: exit.t, kind: RowKind::Exit, text: describe_exit(exit), command: Some(exit.id) });
    }
    if let Some(end) = &session.end {
        rows.push(Row {
            t: end.t,
            kind: RowKind::SessionEnd,
            text: match end.code {
                Some(code) => format!("{} ({code})", end.reason),
                None => end.reason.clone(),
            },
            command: None,
        });
    }

    sort_rows(&mut rows);
    rows
}

fn raw_rows(session: &Session) -> Vec<Row> {
    let mut rows = Vec::new();
    // Walk the timeline and the event list together by timestamp so output
    // lands in the same ordering as the commands that surround it.
    let mut commands = session.commands.iter().peekable();
    for item in &session.timeline {
        let current = commands.peek().filter(|c| c.t <= item.t()).map(|c| c.id);
        let row = match item {
            Item::Output { range, .. } => Row {
                t: item.t(),
                kind: RowKind::Output,
                text: flatten(&strip_ansi(&session.output()[range.clone()]), TEXT_WIDTH),
                command: current,
            },
            Item::Resize { t, size } => Row {
                t: *t,
                kind: RowKind::Resize,
                text: format!("{}x{}", size.cols, size.rows),
                command: None,
            },
        };
        rows.push(row);
    }
    for command in commands {
        rows.push(Row {
            t: command.t,
            kind: if command.exit.is_some_and(|e| e.is_failure()) { RowKind::Error } else { RowKind::Command },
            text: flatten(&command.text, TEXT_WIDTH),
            command: Some(command.id),
        });
    }
    for event in &session.events {
        match event {
            Event::Input { t, .. } => rows.push(Row { t: *t, kind: RowKind::Input, text: "<keystrokes>".into(), command: None }),
            Event::Mark { t, label, .. } => rows.push(Row { t: *t, kind: RowKind::Mark, text: label.clone(), command: None }),
            Event::Signal { t, name, .. } => rows.push(Row { t: *t, kind: RowKind::Signal, text: name.clone(), command: None }),
            Event::Unknown(raw) => {
                if let Some(t) = unknown_t(raw.get()) {
                    rows.push(Row { t, kind: RowKind::Mark, text: format!("unknown event: {}", truncate_chars(raw.get(), 60)), command: None });
                }
            }
            _ => {}
        }
    }
    sort_rows(&mut rows);
    rows
}

/// Best-effort timestamp for an event this build does not understand.
fn unknown_t(raw: &str) -> Option<f64> {
    serde_json::from_str::<serde_json::Value>(raw).ok()?.get("t")?.as_f64()
}

/// A stable sort: equal timestamps keep file order, which is the order they
/// actually happened in.
fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

fn describe_exit(exit: &Exit) -> String {
    match (exit.code, exit.signal) {
        // A silent success is the normal case; spelling it `ok` keeps the
        // timeline scannable instead of a column of `exit 0`.
        (Some(0), _) => "ok".into(),
        _ => exit.describe(),
    }
}

/// Apply the flags the user passed.
///
/// With no flags every row is shown. `--errors` and `--commands` select which
/// command rows survive; asking for both means "the command-related rows",
/// which is what someone who types both is describing.
pub fn filter_rows(rows: Vec<Row>, args: &InspectArgs) -> Vec<Row> {
    let keep = |row: &Row| match (args.errors, args.commands) {
        (true, false) => row.kind == RowKind::Error,
        (false, true) => matches!(row.kind, RowKind::Command | RowKind::Error),
        (true, true) => matches!(row.kind, RowKind::Command | RowKind::Error | RowKind::Exit),
        (false, false) => true,
    };
    rows.into_iter().filter(keep).collect()
}

/// Render a timeline, with a header describing the session.
///
/// When a session mixes commands the shell reported (OSC 133) with commands
/// reconstructed from echo, the reconstructed ones are marked `~`, because a
/// reconstruction is a guess and the difference matters. When every command
/// came from the same source the marker is dropped — a column of identical
/// `~`s is noise, not information.
pub fn format_timeline(session: &Session, rows: &[Row]) -> String {
    let size = session.recorded_size();
    let mixed = session.commands.iter().any(|c| c.source == CommandSource::Echo)
        && session.commands.iter().any(|c| c.source == CommandSource::Integration);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  {}  {}  {}  {}x{}  {}",
        session.meta.id,
        format_rfc3339(session.meta.created_at_unix),
        format_duration(session.duration),
        session.meta.shell.name,
        size.cols,
        size.rows,
        if session.is_incomplete() { "(unfinished)" } else { "" }
    );
    if let Some(title) = &session.meta.title {
        let _ = writeln!(out, "{title}");
    }
    if session.malformed > 0 || session.unknown > 0 {
        let _ = writeln!(
            out,
            "note: {} unreadable line(s), {} unknown event(s) preserved",
            session.malformed, session.unknown
        );
    }
    if mixed {
        let _ = writeln!(out, "note: `~` marks a command detected by echo, not reported by the shell");
    }
    let _ = writeln!(out);

    for row in rows {
        let mark = if mixed && matches!(row.kind, RowKind::Command | RowKind::Error) {
            match row.command.and_then(|id| session.commands.iter().find(|c| c.id == id)) {
                Some(c) if c.source == CommandSource::Echo => "~",
                _ => "",
            }
        } else {
            ""
        };
        let _ = writeln!(out, "{}  {:<6}  {}{}", format_offset(row.t), row.kind.label(), mark, row.text);
    }
    if rows.is_empty() {
        let _ = writeln!(out, "(nothing matched)");
    }
    out
}

/// The screen as it looked when each command was run.
///
/// This is the "what did I have in front of me" view, and it is the reason the
/// session layer keeps a VT model around: reconstructing that from the byte
/// stream at display time would be the same work, done later and worse.
pub fn format_screen_at_commands(session: &Session, size: Option<Size>) -> String {
    let mut out = String::new();
    for command in &session.commands {
        let _ = writeln!(out, "── {} {}", format_offset(command.t), truncate_chars(&command.text, 60));
        for line in session.screen_text_at(command.t, size) {
            let line = line.trim_end();
            if !line.is_empty() {
                let _ = writeln!(out, "  │ {line}");
            }
        }
    }
    out
}

/// Collapse a control-character-laden blob into one readable line.
fn flatten(text: &str, max: usize) -> String {
    let stripped = strip_ansi(text.as_bytes());
    let cleaned: String = stripped.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One row of `eye list`.
pub struct ListEntry {
    pub path: PathBuf,
    pub summary: Option<SessionSummary>,
    /// Why the file could not be summarised. Shown rather than skipped: a
    /// recording that cannot be read is exactly what a user listing their
    /// sessions needs to know about.
    pub problem: Option<String>,
}

pub fn list(dir: &Path) -> Vec<ListEntry> {
    storage::list_sessions(dir)
        .into_iter()
        .map(|path| match SessionSummary::scan(&path) {
            Ok(summary) => ListEntry { path, summary: Some(summary), problem: None },
            Err(e) => ListEntry { path, summary: None, problem: Some(e.to_string()) },
        })
        .collect()
}

pub fn format_list(entries: &[ListEntry]) -> String {
    if entries.is_empty() {
        return "no recordings yet. run `eye` to record one.\n".into();
    }
    let name_width = entries
        .iter()
        .map(|e| e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().len())
        .max()
        .unwrap_or(10)
        .clamp(8, 40);

    let mut out = String::new();
    for entry in entries {
        let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = format!("{name:<name_width$}");
        match (&entry.summary, &entry.problem) {
            (Some(s), None) => {
                let mut notes = String::new();
                if s.incomplete {
                    notes.push_str(" unfinished");
                }
                if s.malformed > 0 {
                    let _ = write!(notes, " {} bad line(s)", s.malformed);
                }
                if s.has_failure() {
                    notes.push_str(" failed command");
                }
                if s.commands > 0 && s.commands as usize > s.exits.len() {
                    let _ = write!(notes, " {} unfinished", s.commands as usize - s.exits.len());
                }
                let _ = writeln!(
                    out,
                    "{name}  {}  {:>7}  {:>2} cmds  {:>8}{}",
                    format_rfc3339(s.meta.created_at_unix).replace('T', " ").replace('Z', ""),
                    format_duration(s.duration),
                    s.commands,
                    human_bytes(s.output_bytes),
                    if notes.is_empty() { String::new() } else { format!("  ({})", notes.trim_start()) },
                );
            }
            _ => {
                let _ = writeln!(out, "{name}  unreadable: {}", entry.problem.as_deref().unwrap_or("?"));
            }
        }
    }
    out
}

/// `1.2 MB`, not `1234567`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// One match.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    /// When the matching text reached the terminal.
    pub t: f64,
    pub line: String,
    /// Context lines, when `--context` asked for them.
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Search the given recordings, or every recording in `dir` if none were
/// named.
pub fn search(args: &SearchArgs, dir: &Path) -> Result<Vec<Hit>> {
    let files: Vec<PathBuf> = if args.files.is_empty() {
        storage::list_sessions(dir)
    } else {
        args.files.iter().map(|f| storage::resolve_session(&f.display().to_string())).collect()
    };
    if files.is_empty() {
        bail!("no recordings to search; run `eye` to record one");
    }

    let mut hits = Vec::new();
    let total = files.len();
    for path in files {
        let session = Session::load(&path)
            .with_context(|| format!("read the recording at {}", path.display()))?;
        if args.screen {
            search_screen(&session, &path, args, &mut hits);
        } else {
            search_stream(&session, &path, args, &mut hits);
        }
    }
    hits.sort_by(|a, b| {
        a.path.cmp(&b.path).then_with(|| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal))
    });
    let _ = total;
    Ok(hits)
}

fn find(haystack: &str, needle: &str, case_sensitive: bool) -> Option<usize> {
    if case_sensitive {
        haystack.find(needle)
    } else {
        haystack.to_lowercase().find(&needle.to_lowercase())
    }
}

/// Search the raw byte stream, line by line.
///
/// Cheap, and closest to "what was written" — but a match whose escape codes
/// were split across a chunk boundary is missed, and text that was drawn and
/// then erased still counts. That is a real limitation, not a bug; `--screen`
/// is the honest alternative.
fn search_stream(session: &Session, path: &Path, args: &SearchArgs, hits: &mut Vec<Hit>) {
    // A pty read almost always splits mid-line, so lines are reassembled
    // across chunks before being searched. Without this, `grep`-style results
    // would be shredded at whatever boundary the kernel happened to use.
    let mut pending = String::new();
    let mut last_t: Option<f64> = None;
    for item in &session.timeline {
        let Item::Output { range, .. } = item else { continue };
        last_t = Some(item.t());
        pending.push_str(&strip_ansi(&session.output()[range.clone()]));
        while let Some(pos) = pending.find('\n') {
            let line: String = pending.drain(..=pos).collect();
            record_hit(&line, last_t.unwrap_or(0.0), path, args, hits);
        }
        // A program that writes megabytes without a newline would otherwise
        // make this quadratic. Report and drop the overflow instead.
        if pending.len() > MAX_PENDING_LINE {
            record_hit(&pending, last_t.unwrap_or(0.0), path, args, hits);
            pending.clear();
        }
    }
    // Output with no trailing newline still reached the screen.
    record_hit(&pending, last_t.unwrap_or(0.0), path, args, hits);
}

/// Reassembly buffer limit for stream search.
const MAX_PENDING_LINE: usize = 64 * 1024;

fn record_hit(line: &str, t: f64, path: &Path, args: &SearchArgs, hits: &mut Vec<Hit>) {
    let line = line.trim_end_matches(['\r', '\n']).trim_end();
    if line.is_empty() {
        return;
    }
    if find(line, &args.query, args.case_sensitive).is_none() {
        return;
    }
    hits.push(Hit {
        path: path.to_path_buf(),
        t,
        line: truncate_chars(line, 160),
        before: None,
        after: None,
    });
}

/// Search what the terminal actually displayed.
///
/// The truthful answer to "was this ever on my screen", at the cost of running
/// the VT model over the whole session. A line is only reported when it
/// *changes* to contain the match, so a progress bar that repaints the same row
/// a thousand times yields one hit rather than a thousand.
fn search_screen(session: &Session, path: &Path, args: &SearchArgs, hits: &mut Vec<Hit>) {
    let size = session.recorded_size();
    let mut screen = Screen::new(size);
    let mut previous: Vec<String> = vec![String::new(); size.rows as usize];
    for item in &session.timeline {
        let t = item.t();
        match item {
            Item::Output { range, .. } => {
                screen.feed(&session.output()[range.clone()]);
                let rows = screen.text_rows();
                for (row, line) in rows.iter().enumerate() {
                    let trimmed = line.trim_end();
                    if trimmed == previous[row] {
                        continue;
                    }
                    if find(trimmed, &args.query, args.case_sensitive).is_some() {
                        hits.push(Hit {
                            path: path.to_path_buf(),
                            t,
                            line: truncate_chars(trimmed, 160),
                            before: (args.context > 0).then(|| context_before(&rows, row, args.context)),
                            after: None,
                        });
                    }
                }
                for (slot, line) in previous.iter_mut().zip(rows.iter()) {
                    *slot = line.trim_end().to_string();
                }
            }
            Item::Resize { size, .. } => {
                screen.resize(*size);
                previous = vec![String::new(); size.rows as usize];
            }
        }
    }
}

fn context_before(rows: &[String], at: usize, context: usize) -> String {
    let from = at.saturating_sub(context);
    rows[from..at]
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_hits(hits: &[Hit], total_files: usize) -> String {
    if hits.is_empty() {
        return format!("no matches in {total_files} recording(s)\n");
    }
    let mut out = String::new();
    let mut last_path: Option<&Path> = None;
    for hit in hits {
        if last_path != Some(hit.path.as_path()) {
            let _ = writeln!(out, "{}", hit.path.display());
            last_path = Some(hit.path.as_path());
        }
        let _ = writeln!(out, "  {}  {}", format_offset(hit.t), hit.line);
        if let Some(before) = &hit.before {
            for line in before.lines() {
                let _ = writeln!(out, "    │   {line}");
            }
        }
        if let Some(after) = &hit.after {
            for line in after.lines() {
                let _ = writeln!(out, "    │   {line}");
            }
        }
    }
    let _ = writeln!(out, "{} match(es) in {total_files} recording(s)", hits.len());
    out
}

/// Does this chunk of text look like an error message?
///
/// A guess, and only ever used to *flag* output for the reader — never to
/// decide what happened. On a pty, stderr and stdout are the same stream, so
/// spelling is the only signal available.
pub fn looks_like_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    const MARKERS: &[&str] = &[
        "error:", "error ", "fatal:", "panic:", "panicked", "traceback (most recent call last)",
        "exception", "no such file", "permission denied", "command not found",
        "segmentation fault", "assertion failed", "warning:",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// Render a session in `format`.
///
/// Returns bytes rather than writing, so writing to a file and writing to
/// stdout are the same code path and `--output` is genuinely optional.
pub fn export(session: &Session, format: ExportFormat, size: Option<Size>) -> Result<Vec<u8>> {
    Ok(match format {
        ExportFormat::Json => export_json(session),
        ExportFormat::Text => export_text(session).into_bytes(),
        ExportFormat::Html => export_html(session, size).into_bytes(),
        ExportFormat::Asciinema => export_asciinema(session),
    })
}

/// The events as JSON, one object per line, exactly as stored.
///
/// The lossless view: unknown event types keep their original bytes, so an
/// export can be handed back to `eye` or to anything else that understands the
/// format.
fn export_json(session: &Session) -> Vec<u8> {
    let mut out = Vec::with_capacity(session.output_bytes() as usize * 2 + 4096);
    out.extend_from_slice(br#"{"type":"meta","meta":"#);
    out.extend_from_slice(serde_json::to_string(&session.meta).unwrap_or_else(|_| "{}".into()).as_bytes());
    out.push(b'\n');
    for event in &session.events {
        if let Ok(line) = crate::event::to_json_line(event) {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    out
}

/// A readable transcript: each command, then the output that followed it.
fn export_text(session: &Session) -> String {
    let size = session.recorded_size();
    let mut out = String::new();
    let _ = writeln!(out, "eye session {}", session.meta.id);
    let _ = writeln!(out, "recorded  {}", format_rfc3339(session.meta.created_at_unix));
    let _ = writeln!(out, "duration  {}", format_duration(session.duration));
    let _ = writeln!(out, "shell     {} ({})", session.meta.shell.name, session.meta.shell.path);
    let _ = writeln!(out, "terminal  {} {}x{}", session.meta.terminal.term, size.cols, size.rows);
    if let Some(cwd) = &session.meta.cwd {
        let _ = writeln!(out, "cwd       {cwd}");
    }
    if let Some(title) = &session.meta.title {
        let _ = writeln!(out, "title     {title}");
    }
    let _ = writeln!(out, "{}", "-".repeat(72));

    // Attribute each command the output that arrived between the previous
    // command and it. The boundary is the command's own timestamp, so the
    // shell's echo of a command line lands with the command it echoes, which
    // is how a transcript is supposed to read.
    let mut boundary = 0usize;
    for command in &session.commands {
        let upto = session.items_upto(command.t);
        let _ = writeln!(out, "\n$ {}", first_line(&command.text));
        write_block(&mut out, &session.output_between(boundary, upto));
        match &command.exit {
            Some(exit) if exit.is_failure() => {
                let _ = writeln!(out, "  [{}]", describe_exit(exit));
            }
            None => {
                let _ = writeln!(out, "  [no exit recorded]");
            }
            Some(_) => {}
        }
        boundary = upto;
    }
    let tail = session.output_between(boundary, session.timeline.len());
    if !strip_ansi(&tail).trim().is_empty() {
        let _ = writeln!(out);
        write_block(&mut out, &tail);
    }
    out
}

/// Write terminal output as an indented transcript block.
///
/// Escape codes are stripped and blank runs collapsed: a transcript is for
/// reading, and a screenful of padding is not content.
fn write_block(out: &mut String, bytes: &[u8]) {
    let text = strip_ansi(bytes);
    let mut blank = 0;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        let _ = writeln!(out, "  {line}");
    }
}

/// A single self-contained HTML page showing the final screen, with colours.
///
/// Built from the VT model rather than the byte stream, so a program that
/// drew, scrolled and cleared leaves exactly the frame a viewer would have
/// seen — and that frame is the thing worth exporting. No external CSS, no
/// fonts, no scripts: the file has to work when opened from disk, offline.
fn export_html(session: &Session, size: Option<Size>) -> String {
    let size = size.unwrap_or_else(|| session.recorded_size());
    let screen = session.screen(Some(size));

    let mut page = String::with_capacity(64 * 1024);
    page.push_str(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>",
    );
    page.push_str(&html_escape(&html_title(session)));
    page.push_str("</title>\n<style>\n\
         :root { color-scheme: dark light; }\n\
         body { background: #111; color: #ddd; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; margin: 0; padding: 2rem; }\n\
         .meta { color: #8a8a8a; font-size: 0.8rem; margin-bottom: 1.25rem; }\n\
         .screen { display: inline-block; background: #000; padding: 0.75rem 1rem; border-radius: 6px; overflow-x: auto; }\n\
         pre { margin: 0; line-height: 1.25; white-space: pre; font-size: 0.9rem; }\n\
         </style>\n</head>\n<body>\n",
    );
    let _ = write!(
        page,
        "<div class=\"meta\">{} &middot; {} &middot; {} &middot; {} &middot; {}x{} &middot; shell {}</div>\n",
        html_escape(&format_rfc3339(session.meta.created_at_unix)),
        format_duration(session.duration),
        session.commands.len(),
        html_escape(&session.meta.terminal.term),
        size.cols,
        size.rows,
        html_escape(&session.meta.shell.name),
    );
    page.push_str("<div class=\"screen\"><pre>");

    for row in 0..size.rows {
        page.push_str(&html_row(&screen, row));
        page.push('\n');
    }
    page.push_str("</pre></div>\n</body>\n</html>\n");
    page
}

fn html_title(session: &Session) -> String {
    match &session.meta.title {
        Some(title) => format!("{title} — eye"),
        None => format!("eye session {}", session.meta.id),
    }
}

/// One grid row as HTML, with runs of identical styling collapsed into spans.
fn html_row(screen: &Screen, row: u16) -> String {
    let mut out = String::new();
    let mut open: Option<(Color, Color, Attrs)> = None;
    let mut text = String::new();

    let flush = |out: &mut String, text: &mut String, open: &mut Option<(Color, Color, Attrs)>| {        if text.is_empty() {
            return;
        }
        let style = match *open {
            Some((fg, bg, attrs)) => cell_style(fg, bg, attrs),
            None => String::new(),
        };
        if style.is_empty() {
            out.push_str(&html_escape(text));
        } else {
            let _ = write!(out, "<span style=\"{style}\">{}</span>", html_escape(text));
        }
        text.clear();
    };

    for cell in screen.row_cells(row) {
        // A zero-width cell is the right half of a wide glyph; emitting it
        // would duplicate the character.
        if cell.width == 0 {
            continue;
        }
        let key = (cell.fg, cell.bg, cell.attrs);
        if open != Some(key) {
            flush(&mut out, &mut text, &mut open);
            open = Some(key);
        }
        text.push(cell.ch);
    }
    flush(&mut out, &mut text, &mut open);
    out
}

fn cell_style(fg: Color, bg: Color, attrs: Attrs) -> String {
    let mut style = String::new();
    if !fg.is_default() {
        let _ = write!(style, "color:{}", fg.to_css("inherit"));
    }
    if !bg.is_default() {
        let _ = write!(style, ";background:{}", bg.to_css("transparent"));
    }
    if style.is_empty() {
        // An empty style attribute is noise; only open a span when it says
        // something.
        if attrs.is_plain() {
            return String::new();
        }
        style.push_str("color:inherit");
    }
    if attrs.contains(Attrs::BOLD) {
        style.push_str(";font-weight:bold");
    }
    if attrs.contains(Attrs::FAINT) {
        style.push_str(";opacity:.6");
    }
    if attrs.contains(Attrs::ITALIC) {
        style.push_str(";font-style:italic");
    }
    if attrs.contains(Attrs::UNDERLINE) {
        style.push_str(";text-decoration:underline");
    }
    if attrs.contains(Attrs::STRIKE) {
        style.push_str(";text-decoration:line-through");
    }
    if attrs.contains(Attrs::INVERSE) {
        style.push_str(";filter:invert(1)");
    }
    style
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// An asciinema v2 cast: a JSON header line, then one event per output chunk.
///
/// The format's timing is a relative delay, so the conversion is a subtraction
/// and nothing else. This is what makes the format worth supporting: a
/// recording is portable to tools that already exist.
fn export_asciinema(session: &Session) -> Vec<u8> {
    let size = session.recorded_size();
    let header = serde_json::json!({
        "version": 2,
        "width": size.cols,
        "height": size.rows,
        "timestamp": session.meta.created_at_unix.round() as i64,
        "title": session.meta.title.clone().unwrap_or_else(|| format!("eye {}", session.meta.id)),
        "env": {
            "SHELL": &session.meta.shell.name,
            "TERM": &session.meta.terminal.term,
        },
    });
    let mut out = header.to_string().into_bytes();
    out.push(b'\n');

    let mut previous = 0.0f64;
    for item in &session.timeline {
        let Item::Output { range, t, .. } = item else { continue };
        let data = &session.output()[range.clone()];
        if data.is_empty() {
            continue;
        }
        let line = serde_json::json!([(t - previous).max(0.0), "o", String::from_utf8_lossy(data)]);
        previous = *t;
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Write an export to a file or to stdout.
pub fn write_export(bytes: &[u8], output: Option<&Path>) -> Result<()> {
    match output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
            }
            std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
        }
        None => {
            let mut out = std::io::stdout().lock();
            out.write_all(bytes)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The end-of-session line: what was recorded, and where.
pub fn recorded_line(path: &Path, duration: f64) -> String {
    format!(
        "eye recorded {}  ·  {}",
        format_duration(duration),
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Bytes, Event, Stream};
    use crate::testsupport;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eye-v-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn out(t: f64, text: &str) -> Event {
        Event::Output { t, data: Bytes::from(text.as_bytes().to_vec()), stream: Stream::Tty }
    }

    fn session(name: &str, events: &[Event]) -> (PathBuf, Session) {
        let path = temp_dir(name).join("s.eye");
        let mut w = storage::SessionWriter::create(&path, testsupport::meta()).unwrap();
        for e in events {
            w.push(e).unwrap();
        }
        w.finish("shell_exit", Some(0)).unwrap();
        let session = Session::load(&path).unwrap();
        (path, session)
    }

    fn args() -> InspectArgs {
        InspectArgs { file: PathBuf::new(), errors: false, commands: false, timeline: false, screen: false }
    }

    /// The directory a fixture session was written into. Tests must not call
    /// `temp_dir` again with the same name: it wipes the directory, deleting
    /// the very file they are about to search.
    fn parent_of(path: &Path) -> PathBuf {
        path.parent().expect("fixture session has a parent").to_path_buf()
    }

    fn search_args(query: &str, files: Vec<PathBuf>) -> SearchArgs {
        SearchArgs { query: query.into(), files, case_sensitive: false, screen: false, context: 0 }
    }

    #[test]
    fn the_timeline_shows_commands_and_their_exits() {
        let (_p, s) = session(
            "tl",
            &[
                out(0.0, "$ ls\r\n"),
                Event::Command { t: 1.0, id: 1, text: "ls".into(), source: CommandSource::Echo },
                Event::Exit { t: 1.5, id: 1, code: Some(0), signal: None },
                Event::Command { t: 2.0, id: 2, text: "false".into(), source: CommandSource::Echo },
                Event::Exit { t: 2.5, id: 2, code: Some(1), signal: None },
            ],
        );
        let text = format_timeline(&s, &inspect_rows(&s, &args()));
        assert!(text.contains("00:01.000  cmd     ls"), "{text}");
        assert!(text.contains("00:02.000  error   false"), "{text}");
        assert!(text.contains("00:01.500  exit    ok"), "{text}");
        assert!(text.contains("00:02.500  exit    exit 1"), "{text}");
    }

    #[test]
    fn a_failing_command_appears_once_not_twice() {
        let (_p, s) = session(
            "once",
            &[
                Event::Command { t: 1.0, id: 1, text: "false".into(), source: CommandSource::Echo },
                Event::Exit { t: 1.5, id: 1, code: Some(1), signal: None },
            ],
        );
        let rows = inspect_rows(&s, &args());
        let errors = rows.iter().filter(|r| r.text.contains("false")).count();
        assert_eq!(errors, 1, "the command row carries the failure");
    }

    #[test]
    fn errors_filter_keeps_only_failures() {
        let (_p, s) = session(
            "err",
            &[
                Event::Command { t: 1.0, id: 1, text: "good".into(), source: CommandSource::Echo },
                Event::Exit { t: 1.5, id: 1, code: Some(0), signal: None },
                Event::Command { t: 2.0, id: 2, text: "bad".into(), source: CommandSource::Echo },
                Event::Exit { t: 2.5, id: 2, code: Some(2), signal: None },
            ],
        );
        let a = InspectArgs { errors: true, ..args() };
        let text = format_timeline(&s, &filter_rows(inspect_rows(&s, &a), &a));
        assert!(text.contains("bad"), "{text}");
        assert!(!text.contains("good"), "{text}");
    }

    #[test]
    fn the_timeline_flag_includes_raw_output() {
        let (_p, s) = session("raw", &[out(0.5, "\x1b[32mgreen\x1b[0m text")]);
        let a = InspectArgs { timeline: true, ..args() };
        let text = format_timeline(&s, &inspect_rows(&s, &a));
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("green text"), "escape codes must be stripped: {text}");
    }

    #[test]
    fn rows_come_out_in_time_order() {
        let (_p, s) = session(
            "order",
            &[
                Event::Resize { t: 5.0, size: Size::new(120, 40) },
                Event::Command { t: 1.0, id: 1, text: "late arrival".into(), source: CommandSource::Echo },
                Event::Exit { t: 0.5, id: 1, code: Some(0), signal: None },
            ],
        );
        let rows = inspect_rows(&s, &args());
        let times: Vec<f64> = rows.iter().map(|r| r.t).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(times, sorted);
    }

    #[test]
    fn resizes_and_signals_appear_in_the_timeline() {
        let (_p, s) = session(
            "resize",
            &[
                out(0.0, "a"),
                Event::Resize { t: 3.0, size: Size::new(120, 40) },
                Event::Signal { t: 4.0, name: "SIGWINCH".into(), number: 28 },
            ],
        );
        let text = format_timeline(&s, &inspect_rows(&s, &args()));
        assert!(text.contains("00:03.000  size    120x40"), "{text}");
        assert!(text.contains("00:04.000  signal  SIGWINCH"), "{text}");
    }

    #[test]
    fn an_unfinished_recording_says_so() {
        let path = temp_dir("unfin").join("s.eye");
        let mut w = storage::SessionWriter::create(&path, testsupport::meta()).unwrap();
        w.push(&out(0.0, "hi")).unwrap();
        // No `finish`: the writer never renames, so only the `.part` exists.
        // That is the crash-safe path, and a session found in it must load.
        assert!(!path.exists(), "an unfinished session must not claim the final name");
        let s = Session::load(w.part_path()).unwrap();
        assert!(s.is_incomplete());
        assert!(format_timeline(&s, &[]).contains("unfinished"));
    }

    #[test]
    fn json_export_is_the_lossless_view() {
        let (_p, s) = session(
            "json",
            &[out(0.0, "hello"), Event::unknown(r#"{"type":"future","t":1.0,"x":1}"#).unwrap()],
        );
        let text = String::from_utf8(export(&s, ExportFormat::Json, None).unwrap()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with(r#"{"type":"meta","meta":"#));
        assert!(lines.iter().any(|l| l.contains(r#""type":"output""#)));
        // The unknown event survives with its original bytes intact.
        assert!(lines.iter().any(|l| *l == r#"{"type":"future","t":1.0,"x":1}"#), "{text}");
    }

    #[test]
    fn text_export_reads_like_a_transcript() {
        let (_p, s) = session(
            "text",
            &[
                out(0.0, "boot noise\r\n"),
                Event::Command { t: 1.0, id: 1, text: "ls -la".into(), source: CommandSource::Echo },
                out(1.2, "\x1b[0mfile1\r\nfile2\r\n"),
                Event::Exit { t: 1.5, id: 1, code: Some(0), signal: None },
            ],
        );
        let text = String::from_utf8(export(&s, ExportFormat::Text, None).unwrap()).unwrap();
        assert!(text.contains("$ ls -la"), "{text}");
        assert!(text.contains("file1") && text.contains("file2"), "{text}");
        assert!(!text.contains('\x1b'), "escape codes must not reach a transcript: {text}");
    }

    #[test]
    fn text_export_flags_a_command_that_failed() {
        let (_p, s) = session(
            "failtxt",
            &[
                Event::Command { t: 1.0, id: 1, text: "rm /".into(), source: CommandSource::Echo },
                out(1.1, "permission denied"),
                Event::Exit { t: 1.5, id: 1, code: Some(1), signal: None },
            ],
        );
        let text = String::from_utf8(export(&s, ExportFormat::Text, None).unwrap()).unwrap();
        assert!(text.contains("[exit 1]"), "{text}");
    }

    #[test]
    fn asciinema_export_is_a_valid_v2_cast() {
        let (_p, s) = session(
            "cast",
            &[
                Event::SessionStart { t: 0.0, size: Size::new(100, 30) },
                out(0.0, "a"),
                out(1.5, "b"),
            ],
        );
        let text = String::from_utf8(export(&s, ExportFormat::Asciinema, None).unwrap()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 100);
        assert_eq!(header["height"], 30);

        let first: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(first[0].as_f64().unwrap(), 0.0);
        assert_eq!(first[1].as_str().unwrap(), "o");
        assert_eq!(first[2].as_str().unwrap(), "a");
        // Delays are relative, so they accumulate back to the recorded times.
        assert_eq!(second[0].as_f64().unwrap(), 1.5);
        assert_eq!(second[2].as_str().unwrap(), "b");
    }

    #[test]
    fn html_export_is_self_contained_and_escaped() {
        let (_p, s) = session(
            "html",
            &[
                out(0.0, "\x1b[31mred & <bold>\x1b[0m"),
                Event::unknown(r#"{"type":"<script>alert(1)</script>"}"#).unwrap(),
            ],
        );
        let text = export_html(&s, None);
        assert!(text.starts_with("<!doctype html>"));
        assert!(text.contains("color:#"), "colour must survive into the page: {text}");
        assert!(text.contains("&lt;bold&gt;"), "terminal text must be escaped");
        assert!(!text.contains("<script>"), "a recording must not be able to inject script");
        // No external anything: the file has to work opened from disk.
        assert!(!text.contains("http://") && !text.contains("https://"));
    }

    #[test]
    fn html_export_skips_the_right_half_of_wide_characters() {
        // A wide glyph occupies two cells; the second has width 0. Emitting it
        // would print the character twice.
        let (_p, s) = session("wide", &[out(0.0, "日本")]);
        let text = export_html(&s, None);
        assert_eq!(text.matches("日").count(), 1, "{text}");
        assert_eq!(text.matches("本").count(), 1, "{text}");
    }

    #[test]
    fn search_finds_text_and_reports_when_it_finds_nothing() {
        let (path, _s) = session(
            "search",
            &[out(0.0, "compiling module one\n"), out(2.0, "error: undefined\n")],
        );
        let args = SearchArgs { context: 1, ..search_args("error", vec![path.clone()]) };
        let dir = parent_of(&path);
        let hits = search(&args, &dir).unwrap();
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(hits[0].t, 2.0);

        let none = search_args("nowhere", vec![path]);
        assert!(search(&none, &dir).unwrap().is_empty());
        assert!(format_hits(&[], 1).contains("no matches"));
    }

    #[test]
    fn search_is_case_insensitive_unless_asked() {
        let (path, _s) = session("case", &[out(0.0, "Permission denied\n")]);
        let dir = parent_of(&path);
        assert_eq!(search(&search_args("permission", vec![path.clone()]), &dir).unwrap().len(), 1);
        let exact = SearchArgs { case_sensitive: true, ..search_args("permission", vec![path]) };
        assert!(search(&exact, &dir).unwrap().is_empty());
    }

    #[test]
    fn screen_search_answers_what_was_actually_displayed() {
        // The screen is examined after every chunk, so text that is written
        // and then overwritten *within one chunk* never appears as a frame and
        // `--screen` does not report it, while a stream search does. That is
        // the honest difference between the two, and it is worth pinning down.
        let (path, _s) = session(
            "sscr",
            &[out(0.0, "secret\x1b[2K\rpublic"), out(1.0, "\nvisible")],
        );
        let dir = parent_of(&path);

        let stream = search(&search_args("secret", vec![path.clone()]), &dir).unwrap();
        assert_eq!(stream.len(), 1, "the bytes were written, so a stream search finds them");

        let screen = SearchArgs { screen: true, ..search_args("secret", vec![path.clone()]) };
        assert!(search(&screen, &dir).unwrap().is_empty(), "it was never a visible frame");

        let shown = SearchArgs { screen: true, ..search_args("public", vec![path]) };
        assert_eq!(search(&shown, &dir).unwrap().len(), 1, "what replaced it was visible");
    }

    #[test]
    fn screen_search_forgets_text_that_scrolled_off() {
        // 30 lines into a 24-row screen in one chunk: the first six rows are
        // gone by the time the chunk is rendered.
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("line{i}\n"));
        }
        let (path, _s) = session("scroll", &[out(0.0, &text)]);
        let dir = parent_of(&path);
        let stream = search(&search_args("line0", vec![path.clone()]), &dir).unwrap();
        assert_eq!(stream.len(), 1, "the stream still has it");
        let screen = SearchArgs { screen: true, ..search_args("line0", vec![path]) };
        assert!(search(&screen, &dir).unwrap().is_empty(), "but it scrolled away");
    }

    #[test]
    fn a_repeated_line_is_reported_once_not_once_per_redraw() {
        // A spinner repainting the same row 5 times must not produce 5 hits.
        let mut events: Vec<Event> = Vec::new();
        for i in 0..5 {
            events.push(out(i as f64, &format!("\r\x1b[2Kloading {i}")));
        }
        let (path, _s) = session("redraw", &events);
        let hits = search(&SearchArgs { screen: true, ..search_args("loading", vec![path.clone()]) }, &parent_of(&path)).unwrap();
        assert!(!hits.is_empty(), "the line changed, so it must be reported");
        assert!(hits.iter().all(|h| h.line.contains("loading")));
    }

    #[test]
    fn list_reports_a_broken_file_rather_than_skipping_it() {
        let (path, _s) = session("listsrc", &[out(0.0, "x")]);
        let dir = parent_of(&path);
        std::fs::write(dir.join("broken.eye"), b"not json at all\n").unwrap();
        std::fs::rename(&path, dir.join("good.eye")).unwrap();

        let entries = list(&dir);
        assert_eq!(entries.len(), 2, "{:?}", entries.iter().map(|e| &e.path).collect::<Vec<_>>());
        let text = format_list(&entries);
        assert!(text.contains("good.eye"), "{text}");
        assert!(text.contains("unreadable"), "{text}");
    }

    #[test]
    fn an_empty_list_says_what_to_do_about_it() {
        assert!(format_list(&list(&temp_dir("emptylist"))).contains("run `eye`"));
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn list_args_default_to_the_session_directory() {
        // Constructed only to keep the type honest; the default lives in main.
        let _ = crate::cli::ListArgs { dir: None };
    }

    #[test]
    fn error_shaped_text_is_flagged() {
        assert!(looks_like_error("bash: foo: command not found"));
        assert!(looks_like_error("Error: cannot open file"));
        assert!(looks_like_error("thread 'main' panicked at"));
        assert!(!looks_like_error("everything is fine"));
    }

    #[test]
    fn control_characters_are_flattened_for_timeline_rows() {
        assert_eq!(flatten("a\x1b[2Jb\r\nc", 100), "ab c");
        assert_eq!(flatten(&"x".repeat(200), 10), format!("{}…", "x".repeat(9)));
    }
}
