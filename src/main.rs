//! The `eye` binary.
//!
//! This file is deliberately thin. It maps "what the user typed" onto the
//! library's entry points, owns the two pieces of terminal chrome (the start
//! banner and the end-of-session line), and turns errors into an exit code.
//! Everything below it — recording, replay, storage, the views in [`report`] —
//! is library code with its own tests, so the parts that matter can be
//! exercised without a terminal.
//!
//! Run `eye` with no arguments to record. That is the whole point of the
//! program, so it is the default rather than a subcommand the user has to know
//! about first.

use clap::Parser;
use eye::cli::{Cli, Command, ExportArgs, InspectArgs, ListArgs, RecordArgs, ReplayArgs, SearchArgs};
use eye::recorder::{self, RecordOptions};
use eye::replay::{self, ReplayOptions, StopReason};
use eye::report;
use eye::session::Session;
use eye::storage;
use eye::util::time::{format_duration, parse_timestamp};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` prints the whole anyhow chain on one line, which is what a
            // person at a shell wants; the alternative is a stack of context
            // sentences they have to reassemble themselves.
            eprintln!("eye: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        // No subcommand means record. The banner is the only thing that
        // distinguishes it from an explicit `eye record`.
        None => record_session(cli, &RecordArgs::default()),
        Some(Command::Record(args)) => record_session(cli, args),
        Some(Command::Replay(args)) => replay_session(cli, args),
        Some(Command::Inspect(args)) => inspect(args),
        Some(Command::List(args)) => list(args),
        Some(Command::Export(args)) => export(cli, args),
        Some(Command::Search(args)) => search(args),
    }
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

fn record_session(cli: &Cli, args: &RecordArgs) -> anyhow::Result<()> {
    let path = match &args.output {
        Some(path) => path.clone(),
        None => storage::default_session_path(),
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
        }
    }

    // The banner goes to the real terminal *before* the session starts, so it
    // is never part of the recording: it is chrome around a session, not
    // content inside one.
    if banner_enabled(cli) {
        print_banner(&path);
    }

    let opts = RecordOptions {
        path: Some(path.clone()),
        shell: args.shell.as_deref().map(|name| eye::recorder::shell::Shell::named(name, &[])),
        cwd: args.cwd.clone(),
        title: args.title.clone(),
        record_input: args.input,
        no_integration: args.no_integration,
        ..RecordOptions::default()
    };

    let outcome = recorder::record(&opts)?;

    // Hand the terminal back before saying anything. A session that ended
    // inside a full-screen program would otherwise leave the user staring at
    // that program's last frame with no way out but `reset`.
    let teardown = replay::teardown_bytes(true);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&teardown);
    let _ = write!(stdout, "\r\n{}\r\n", report::recorded_line(&outcome.path, outcome.duration));
    if cli.verbose {
        let _ = write!(
            stdout,
            "  {} events, ended: {}{}\r\n",
            outcome.events,
            outcome.reason,
            match outcome.exit_code {
                Some(code) => format!(" ({code})"),
                None => String::new(),
            }
        );
    }
    let _ = stdout.flush();
    Ok(())
}

/// Whether the start banner should be drawn.
///
/// `--no-banner` for one run, `EYE_NO_BANNER` for a shell profile: someone
/// scripting `eye` around another program should not have to filter output.
fn banner_enabled(cli: &Cli) -> bool {
    !cli.no_banner && std::env::var_os("EYE_NO_BANNER").is_none()
}

/// The start banner.
///
/// A box, not a logo. It says exactly two things — that `eye` is watching the
/// terminal, and what file the session is being written to — because those are
/// the two facts a user needs and neither is obvious. It is drawn before raw
/// mode and never re-drawn, so it costs one screen line pair and no flicker.
fn print_banner(path: &Path) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    // Two content lines; the box is sized to the longer one. The literal
    // strings are ASCII so the width calculation is honest.
    let left = "watching terminal";
    let right = format!("recording  {name}");
    let width = left.len().max(right.len());
    let title = " eye ";
    let fill = width + 2 - title.len();

    let mut out = String::with_capacity(256);
    // Dim the frame, leave the text at normal weight: the box should recede.
    out.push_str("\r\n\x1b[2m  \u{256d}\u{2500}");
    out.push_str(title);
    for _ in 0..fill {
        out.push('\u{2500}');
    }
    out.push_str("\u{256e}\x1b[0m\r\n");
    out.push_str(&format!("  \x1b[2m\u{2502}\x1b[0m {left:<width$} \x1b[2m\u{2502}\x1b[0m\r\n"));
    out.push_str(&format!("  \x1b[2m\u{2502}\x1b[0m {right:<width$} \x1b[2m\u{2502}\x1b[0m\r\n"));
    out.push_str("  \x1b[2m\u{2570}");
    for _ in 0..fill + title.len() {
        out.push('\u{2500}');
    }
    out.push_str("\u{256f}\x1b[0m\r\n");

    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

// ---------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------

fn replay_session(cli: &Cli, args: &ReplayArgs) -> anyhow::Result<()> {
    let path = storage::resolve_session(&args.file.display().to_string());
    if !path.exists() {
        anyhow::bail!(
            "no recording at {}; run `eye list` to see what there is",
            path.display()
        );
    }

    let speed = match args.speed {
        Some(speed) if speed.is_finite() && speed > 0.0 => speed.clamp(0.05, 128.0),
        Some(speed) => anyhow::bail!("--speed must be a positive number, got {speed}"),
        None => 1.0,
    };
    let start = match &args.start {
        Some(text) => parse_timestamp(text)
            .ok_or_else(|| anyhow::anyhow!("--start wants `90`, `1:30` or `1:02:03`, got {text:?}"))?,
        None => 0.0,
    };

    if banner_enabled(cli) {
        // A one-line note, not the record banner: replay is about to take over
        // the screen, so anything taller would be erased almost immediately.
        let mut stdout = std::io::stdout().lock();
        let _ = write!(
            stdout,
            "\x1b[2meye replay\x1b[0m {}  space pause · \u{2190}/\u{2192} seek · +/- speed · r restart · q quit\r\n",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        );
        let _ = stdout.flush();
    }

    let opts = ReplayOptions {
        path: path.clone(),
        speed,
        start,
        apply_resize: true,
        progress: args.progress,
        ..ReplayOptions::default()
    };

    // Replay draws in the alternate screen on its own; no banner cleanup is
    // needed here, only honesty about how it ended.
    match replay::replay(&opts) {
        Ok(outcome) => {
            if cli.verbose {
                eprintln!(
                    "eye: played {} of {} ({} chunks, {} bytes){}",
                    format_duration(outcome.position),
                    format_duration(outcome.duration),
                    outcome.chunks,
                    outcome.bytes,
                    match &outcome.reason {
                        StopReason::Signal(name) => format!(", interrupted by {name}"),
                        _ => String::new(),
                    }
                );
            }
            Ok(())
        }
        // A recording killed by a signal during replay is not an `eye` bug, so
        // do not dress it up as one; the terminal has already been restored.
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn inspect(args: &InspectArgs) -> anyhow::Result<()> {
    let path = resolve_existing(&args.file)?;
    let session = Session::load(&path)?;
    let rows = report::filter_rows(report::inspect_rows(&session, args), args);
    let mut out = report::format_timeline(&session, &rows);
    if args.screen {
        out.push('\n');
        out.push_str(&report::format_screen_at_commands(&session, None));
    }
    print!("{out}");
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(args: &ListArgs) -> anyhow::Result<()> {
    let dir = args.dir.clone().unwrap_or_else(storage::sessions_dir);
    let entries = report::list(&dir);
    print!("{}", report::format_list(&entries));
    Ok(())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

fn export(cli: &Cli, args: &ExportArgs) -> anyhow::Result<()> {
    let path = resolve_existing(&args.file)?;
    let session = Session::load(&path)?;
    let bytes = report::export(&session, args.format, args.size())?;

    // `-o DIR` writes a sensibly named file inside the directory; `-o FILE`
    // writes exactly that file. Getting this wrong in the other direction (a
    // directory silently replaced by a file) is not worth the guesswork.
    let output: Option<PathBuf> = match &args.output {
        Some(dir) if dir.is_dir() => {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "export".into());
            Some(dir.join(format!("{stem}.{}", args.format.extension())))
        }
        other => other.clone(),
    };

    report::write_export(&bytes, output.as_deref())?;
    if cli.verbose {
        match &output {
            Some(path) => eprintln!("eye: wrote {} ({} bytes) to {}", args.format.as_str(), bytes.len(), path.display()),
            None => eprintln!("eye: wrote {} ({} bytes) to stdout", args.format.as_str(), bytes.len()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

fn search(args: &SearchArgs) -> anyhow::Result<()> {
    let dir = storage::sessions_dir();
    let total = if args.files.is_empty() {
        storage::list_sessions(&dir).len()
    } else {
        args.files.len()
    };
    let hits = report::search(args, &dir)?;
    print!("{}", report::format_hits(&hits, total));
    Ok(())
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

/// Resolve a file argument and complain usefully when it is not there.
///
/// `resolve_session` happily returns a path that does not exist, which is
/// right for it (it does not touch the filesystem for explicit paths) but
/// wrong for a user-facing command.
fn resolve_existing(file: &Path) -> anyhow::Result<PathBuf> {
    let path = storage::resolve_session(&file.display().to_string());
    if !path.exists() {
        anyhow::bail!(
            "no recording at {}; run `eye list` to see what there is",
            path.display()
        );
    }
    Ok(path)
}
