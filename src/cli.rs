//! Command-line surface.
//!
//! Kept separate from `main.rs` so argument parsing can be tested without a
//! terminal, and so the mapping from "what the user typed" to "what the engine
//! does" is one readable function per subcommand.
//!
//! `eye` with no arguments is the interesting case: recording is the default,
//! because that is what a session recorder is *for*. Every other verb is an
//! explicit ask.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// A terminal session recorder that stores sessions as structured, replayable
/// data.
///
/// Run `eye` on its own to record, then `eye replay <file>` to watch it again.
#[derive(Debug, Parser)]
#[command(
    name = "eye",
    version,
    about,
    // The banner is the product; clap should not compete with it.
    disable_help_subcommand = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Do not print the start banner.
    #[arg(long, global = true)]
    pub no_banner: bool,

    /// Print what is happening; useful when driving `eye` from a script.
    #[arg(long, short, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record a terminal session (the default).
    Record(RecordArgs),
    /// Replay a recorded session.
    Replay(ReplayArgs),
    /// Show a recording as a structured timeline.
    Inspect(InspectArgs),
    /// List recorded sessions.
    List(ListArgs),
    /// Export a recording to another format.
    Export(ExportArgs),
    /// Search the text inside recordings.
    Search(SearchArgs),
}

#[derive(Debug, Args, Default)]
pub struct RecordArgs {
    /// Where to write the recording. Defaults to a dated file in the session
    /// directory, and never overwrites an existing one.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Shell to run instead of `$SHELL`.
    #[arg(short = 'S', long, value_name = "SHELL")]
    pub shell: Option<String>,

    /// Working directory for the shell.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Label stored in the recording's header.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// Record keystrokes as well as output.
    #[arg(long)]
    pub input: bool,

    /// Do not install shell integration; detect commands by echo alone.
    #[arg(long)]
    pub no_integration: bool,
}

#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// Recording to play. A bare name is looked up in the session directory.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Playback rate, e.g. 0.5, 2, 8.
    #[arg(long, short, value_name = "RATE")]
    pub speed: Option<f64>,

    /// Start at this timestamp: `90`, `1:30`, `1:02:03`.
    #[arg(long, value_name = "TIME")]
    pub start: Option<String>,

    /// Draw a position and speed readout while replaying.
    #[arg(long)]
    pub progress: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Recording to inspect.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Show only the commands that failed.
    #[arg(long)]
    pub errors: bool,

    /// Show only the commands that were run.
    #[arg(long)]
    pub commands: bool,

    /// Show every event, not just the summary rows.
    #[arg(long)]
    pub timeline: bool,

    /// Also show the terminal text at each command.
    #[arg(long)]
    pub screen: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Directory to list. Defaults to the session directory.
    #[arg(value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Recording to export.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Output format.
    #[arg(long, short, value_name = "FORMAT")]
    pub format: ExportFormat,

    /// Write here instead of to stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Render at this size instead of the recorded one.
    #[arg(long, value_names = ["COLS", "ROWS"], num_args = 2)]
    pub size: Option<Vec<u16>>,
}

impl ExportArgs {
    /// The requested render size, if both dimensions parsed.
    pub fn size(&self) -> Option<crate::event::Size> {
        let dims = self.size.as_ref()?;
        if dims.len() == 2 && dims[0] > 0 && dims[1] > 0 {
            Some(crate::event::Size::new(dims[0], dims[1]))
        } else {
            None
        }
    }
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// What to look for. Treated as a plain substring, not a pattern.
    #[arg(value_name = "TEXT")]
    pub query: String,

    /// Search these recordings instead of all of them.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Case-sensitive search.
    #[arg(short, long)]
    pub case_sensitive: bool,

    /// Search the text the terminal displayed rather than the raw byte stream.
    #[arg(long)]
    pub screen: bool,

    /// Show this many lines of context around each hit.
    #[arg(long, short = 'C', value_name = "N", default_value_t = 0)]
    pub context: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// The events as JSON.
    Json,
    /// A plain-text transcript.
    Text,
    /// A self-contained HTML page with the rendered screen.
    Html,
    /// An asciinema v2 cast, playable at asciinema.org.
    Asciinema,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Text => "text",
            ExportFormat::Html => "html",
            ExportFormat::Asciinema => "asciinema",
        }
    }

    /// The extension an export gets when written to a file without one.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Text => "txt",
            ExportFormat::Html => "html",
            ExportFormat::Asciinema => "cast",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_argument_parser_is_internally_consistent() {
        // Catches duplicate short flags, bad `value_name`s and typos in the
        // derive attributes, none of which the type checker sees.
        Cli::command().debug_assert();
    }

    #[test]
    fn recording_is_what_happens_with_no_arguments() {
        let cli = Cli::parse_from(["eye"]);
        assert!(cli.command.is_none(), "the CLI resolves the default in main, not here");
        assert!(!cli.no_banner);
        assert!(!cli.verbose);
    }

    #[test]
    fn record_accepts_the_usual_shapes() {
        let cli = Cli::parse_from(["eye", "record", "-o", "s.eye", "--title", "build", "--input"]);
        match cli.command {
            Some(Command::Record(args)) => {
                assert_eq!(args.output, Some(PathBuf::from("s.eye")));
                assert_eq!(args.title.as_deref(), Some("build"));
                assert!(args.input);
                assert!(!args.no_integration);
            }
            other => panic!("expected record, got {other:?}"),
        }
    }

    #[test]
    fn replay_takes_a_speed_and_a_timestamp() {
        let cli = Cli::parse_from(["eye", "replay", "s.eye", "--speed", "4", "--start", "1:30"]);
        match cli.command {
            Some(Command::Replay(args)) => {
                assert_eq!(args.file, PathBuf::from("s.eye"));
                assert_eq!(args.speed, Some(4.0));
                assert_eq!(args.start.as_deref(), Some("1:30"));
            }
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn export_formats_parse_from_names() {
        for (text, expected) in [
            ("json", ExportFormat::Json),
            ("text", ExportFormat::Text),
            ("html", ExportFormat::Html),
            ("asciinema", ExportFormat::Asciinema),
        ] {
            let cli = Cli::parse_from(["eye", "export", "s.eye", "-f", text]);
            match cli.command {
                Some(Command::Export(args)) => assert_eq!(args.format, expected),
                other => panic!("expected export, got {other:?}"),
            }
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand_too() {
        let cli = Cli::parse_from(["eye", "replay", "s.eye", "--no-banner", "-v"]);
        assert!(cli.no_banner);
        assert!(cli.verbose);
    }

    #[test]
    fn an_unknown_format_is_rejected_at_parse_time() {
        assert!(Cli::try_parse_from(["eye", "export", "s.eye", "-f", "mp4"]).is_err());
    }

    #[test]
    fn a_file_argument_is_required_where_it_is_the_whole_point() {
        assert!(Cli::try_parse_from(["eye", "replay"]).is_err());
        assert!(Cli::try_parse_from(["eye", "inspect"]).is_err());
        assert!(Cli::try_parse_from(["eye", "search"]).is_err());
    }
}
