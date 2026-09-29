//! Which shell to run, and how to teach it to mark commands.
//!
//! Command boundaries are the one piece of structure a PTY cannot give you for
//! free: bytes arrive with no idea where a command ended or what it returned.
//! There are two ways to recover that, and `eye` uses both.
//!
//! 1. **Shell integration.** Wrap the prompt so the shell emits OSC 133 marks
//!    (`A` prompt start, `B` line accepted, `C` command start, `D` command
//!    finished plus the exit status). Authoritative, including exit codes.
//!    Supported for bash, zsh and fish; anything else is left alone.
//!
//! 2. **Echo reconstruction.** Reconstruct what the user typed from the input
//!    byte stream and confirm it against the terminal's echo. Works with *any*
//!    shell, but cannot recover exit codes.
//!
//! Integration is injected without touching the user's configuration: it goes
//! in through a temporary rcfile / init command that the shell reads in
//! addition to, or instead of, the user's own. The user's own settings are
//! always loaded first, so a user's prompt, aliases and `PS0` behave as they
//! would in a normal terminal — `eye` only adds to what runs, it never
//! replaces it.

use std::path::{Path, PathBuf};

/// What we know about the shell we are about to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    /// Resolved executable path.
    pub path: PathBuf,
    /// Basename, lowercased.
    pub name: String,
    /// Extra arguments before the user's own.
    pub lead_args: Vec<String>,
    /// Does `eye` know how to install OSC 133 hooks here?
    pub integration: Integration,
}

/// How command detection will work for a given shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integration {
    /// OSC 133 marks will be emitted.
    Osc133,
    /// Only echo reconstruction is available.
    Echo,
}

impl Integration {
    pub fn as_str(self) -> &'static str {
        match self {
            Integration::Osc133 => "osc133",
            Integration::Echo => "echo",
        }
    }
}

/// Everything needed to start the shell: the program, its arguments, and the
/// environment changes that have to travel with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    /// Environment variables the invocation itself requires (zsh's `ZDOTDIR`).
    pub env: Vec<(String, String)>,
    pub integration: Integration,
}

/// Find a shell to run.
///
/// `$SHELL` is the user's login shell, which is the right default: running
/// `eye` should feel like opening a new terminal window, not like being handed
/// a different shell than the one they configured.
pub fn detect() -> Result<Shell, String> {
    let from_env = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    let path = match from_env {
        Some(p) if Path::new(&p).exists() => PathBuf::from(p),
        _ => ["/bin/bash", "/usr/bin/fish", "/bin/zsh", "/bin/sh"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .ok_or_else(|| {
                "no shell found: set $SHELL, or name one with `eye record -- <shell>`".to_owned()
            })?,
    };
    Ok(Shell::for_path(path))
}

impl Shell {
    /// Classify a shell by its executable name.
    pub fn for_path(path: PathBuf) -> Shell {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        // `bash5`, `zsh-5.9`, `fish` all reduce to the base name.
        let base: String = name
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '.')
            .to_owned();
        let integration = match base.as_str() {
            "bash" | "zsh" | "fish" => Integration::Osc133,
            _ => Integration::Echo,
        };
        Shell { path, name: base, lead_args: Vec::new(), integration }
    }

    /// A shell named explicitly on the command line.
    pub fn named(program: &str, args: &[String]) -> Shell {
        let mut shell = Shell::for_path(PathBuf::from(program));
        shell.lead_args = args.to_vec();
        shell
    }

    /// True if this is a shell we are willing to start in interactive mode.
    pub fn is_interactive_default(&self) -> bool {
        self.lead_args.is_empty() || self.lead_args.iter().all(|a| a == "-i")
    }

    /// Build the command line for this shell, installing integration hooks.
    ///
    /// `temp` is a scratch directory the shell may write a generated
    /// configuration file into; it stays empty for shells that need none.
    ///
    /// Hooks are *appended*: the user's own configuration is sourced first and
    /// our `PS0` / `PROMPT_COMMAND` / event handlers are installed afterwards,
    /// so a user who deliberately customised those still gets a working
    /// session, and a user who has never heard of OSC 133 is unaffected.
    pub fn invocation(&self, temp: &Path) -> Invocation {
        let program = self.path.display().to_string();
        let integration = self.integration;
        if !self.is_interactive_default() {
            // The user asked for a specific non-interactive mode; injecting
            // `-i` or an rcfile would override them.
            return Invocation {
                program,
                args: self.lead_args.clone(),
                env: Vec::new(),
                integration,
            };
        }

        let mut args: Vec<String> = Vec::new();
        let mut env: Vec<(String, String)> = Vec::new();

        match (self.integration, self.name.as_str()) {
            (Integration::Osc133, "bash") => {
                // `--rcfile` replaces the user's own, so ours sources theirs.
                let rc = temp.join("eye.bashrc");
                let _ = std::fs::write(
                    &rc,
                    bash::rcfile(&user_rc(&[".bashrc", ".bash_profile", ".bash_login"])),
                );
                args.push("--rcfile".into());
                args.push(rc.display().to_string());
            }
            (Integration::Osc133, "zsh") => {
                // zsh finds its configuration through `$ZDOTDIR`, so the
                // generated files have to be a whole directory. It travels in
                // the environment, never as an argument.
                let zdotdir = temp.join("zdotdir");
                let _ = std::fs::create_dir_all(&zdotdir);
                let user = user_rc(&[".zshrc"]);
                for rc in &[".zshenv", ".zprofile", ".zshrc"] {
                    let _ = std::fs::write(zdotdir.join(rc), zsh::rcfile(rc, &user));
                }
                env.push(("ZDOTDIR".into(), zdotdir.display().to_string()));
            }
            (Integration::Osc133, "fish") => {
                // fish reads `--init-command` before the user's config, and
                // fish 3.7+ emits OSC 133 natively, so ours is a fallback for
                // older versions. Both firing is harmless: the tracker
                // collapses duplicate marks.
                args.push(format!("--init-command={}", fish::hooks_oneline()));
            }
            _ => {}
        }

        // Interactive mode is what makes a shell draw a prompt worth
        // integrating with.
        //
        // It goes *after* any long option, not before. `bash -i --rcfile X`
        // fails with "`--`: invalid option" — bash will not accept a long
        // option once a short-option cluster has started — while
        // `bash --rcfile X -i` works. Other shells take either order, so the
        // ordering bash requires is the one to use everywhere.
        if !args.iter().any(|a| a == "-i") && !self.lead_args.iter().any(|a| a == "-i") {
            args.push("-i".into());
        }
        args.extend(self.lead_args.iter().cloned());

        Invocation { program, args, env, integration }
    }

    /// Environment for the child, minus anything `eye` itself set.
    pub fn env(&self, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.retain(|(k, _)| {
            !k.starts_with("EYE_") && k != "ZDOTDIR" && k != "BASH_ENV" && k != "ENV"
        });
        env.extend_from_slice(extra);
        env
    }
}

/// First existing rc file from a list of candidates.
fn user_rc(candidates: &[&str]) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    candidates.iter().map(|n| home.join(n)).find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Shell integration scripts
// ---------------------------------------------------------------------------

/// The OSC 133 marks `eye` understands.
///
/// ```text
/// A  prompt start          B  line accepted / prompt shown
/// C  command start         D  command finished, carrying the exit status
/// ```
///
/// Only `C` and `D` are load-bearing. `B` means different things in different
/// shells — bash and zsh emit it just before a command runs, fish emits it
/// when it draws a prompt — so `eye` keys off `C`, which every one of them
/// emits at the same instant.
pub const MARK: &str = "133";

/// bash integration, appended to a generated rcfile.
pub mod bash {
    /// The hooks themselves.
    pub const HOOKS: &str = r#"
__eye_mark() { printf '\033]133;%s\007' "$1"; }
__eye_postexec() {
  local rc="$1"
  printf '\033]133;D;%s\007' "$rc"
  printf '\033]133;A\007'
}
# Prepend, never append. bash runs a multi-command PROMPT_COMMAND in order and
# $? tracks the previous command *in that list*, so appending would read the
# status of whatever the user's own prompt hook just did — Fedora's default
# sets the window title, whose status is always 0. Going first means $? is
# still the status of the command the user actually ran.
if [ -n "${PROMPT_COMMAND:-}" ]; then
  case ";${PROMPT_COMMAND};" in
    *__eye_postexec*) ;;
    *) PROMPT_COMMAND="__eye_postexec \"\$?\";${PROMPT_COMMAND}" ;;
  esac
else
  PROMPT_COMMAND='__eye_postexec "$?"'
fi
# PS0 is printed immediately before each command is executed (bash >= 4.4),
# which is the earliest point at which a command is known to be starting.
PS0='\033]133;C\007'
PS1='\033]133;A\007'"${PS1:-}"
export EYE=1
"#;

    /// A bash rcfile that loads the user's own configuration and then ours.
    pub fn rcfile(user_rc: &Option<std::path::PathBuf>) -> String {
        let mut out = String::new();
        if let Some(path) = user_rc {
            let p = path.display();
            out.push_str(&format!("if [ -r \"{p}\" ]; then . \"{p}\"; fi\n"));
        }
        out.push_str(HOOKS);
        out
    }
}

/// zsh integration, expressed in a generated `.zshrc`.
pub mod zsh {
    pub const HOOKS: &str = r#"
__eye_mark() { printf '\033]133;%s\007' "$1"; }
__eye_preexec() { __eye_mark C }
__eye_postexec() { __eye_mark "D;$?"; __eye_mark A }
autoload -Uz add-zsh-hook
add-zsh-hook preexec __eye_preexec
add-zsh-hook precmd  __eye_postexec
export EYE=1
"#;

    /// One zsh rc file. `.zshenv` and `.zprofile` stay empty so the user's own
    /// files are loaded exactly once, by `.zshrc`.
    pub fn rcfile(which: &str, user_rc: &Option<std::path::PathBuf>) -> String {
        let mut out = String::new();
        if which == ".zshrc" {
            if let Some(path) = user_rc {
                let p = path.display();
                out.push_str(&format!("if [ -r \"{p}\" ]; then . \"{p}\"; fi\n"));
            }
            out.push_str(HOOKS);
        }
        out
    }
}

/// fish integration via `--init-command`.
///
/// fish 3.7 and later emit OSC 133 on their own, so these hooks only matter
/// for older versions; they are written to be harmless alongside the native
/// implementation. `--init-command` takes a single command string, so the
/// functions are separated with `;` rather than newlines.
pub mod fish {
    /// The same hooks, flattened onto one line.
    pub fn hooks_oneline() -> String {
        HOOKS
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub const HOOKS: &str = r#"
function __eye_mark -a kind
    printf '\033]133;%s\007' $kind
end
function __eye_preexec --on-event fish_preexec
    __eye_mark C
end
function __eye_postexec --on-event fish_postexec
    __eye_mark "D;$argv[1]"
    __eye_mark A
end
set -gx EYE 1
"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eye-shelltest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shells_are_classified_by_base_name() {
        assert_eq!(Shell::for_path("/usr/bin/fish".into()).name, "fish");
        assert_eq!(Shell::for_path("/bin/bash".into()).integration, Integration::Osc133);
        assert_eq!(Shell::for_path("/bin/zsh-5.9".into()).name, "zsh");
        assert_eq!(Shell::for_path("/usr/bin/dash".into()).integration, Integration::Echo);
        assert_eq!(Shell::for_path("/bin/sh".into()).integration, Integration::Echo);
        assert_eq!(Shell::for_path("/bin/nu".into()).integration, Integration::Echo);
    }

    #[test]
    fn unknown_shells_get_a_plain_interactive_invocation() {
        let inv = Shell::for_path("/usr/bin/nu".into()).invocation(Path::new("/tmp"));
        assert_eq!(inv.program, "/usr/bin/nu");
        assert_eq!(inv.args, vec!["-i".to_owned()]);
        assert!(inv.env.is_empty());
    }

    #[test]
    fn bash_gets_an_rcfile_that_loads_the_users_own() {
        let dir = scratch("bash");
        let inv = Shell::for_path("/bin/bash".into()).invocation(&dir);
        assert_eq!(inv.program, "/bin/bash");
        assert!(inv.args.contains(&"--rcfile".to_owned()));
        assert!(inv.args.contains(&"-i".to_owned()));
        let rc = inv.args.iter().find(|a| a.ends_with("eye.bashrc")).expect("rcfile");
        let text = std::fs::read_to_string(rc).unwrap();
        assert!(text.contains(".bashrc"), "the user's own rc must still be sourced");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zsh_gets_a_zdotdir_in_the_environment_not_in_argv() {
        let dir = scratch("zsh");
        let inv = Shell::for_path("/bin/zsh".into()).invocation(&dir);
        assert_eq!(inv.env.len(), 1);
        assert_eq!(inv.env[0].0, "ZDOTDIR");
        assert!(!inv.args.iter().any(|a| a.starts_with("ZDOTDIR=")));
        assert!(inv.args.contains(&"-i".to_owned()));
        let zshrc = Path::new(&inv.env[0].1).join(".zshrc");
        assert!(std::fs::read_to_string(&zshrc).unwrap().contains("add-zsh-hook"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_options_always_come_before_the_interactive_flag() {
        // bash rejects a long option that follows a short-option cluster:
        // `bash -i --rcfile X` dies with "`--`: invalid option" and the
        // session never starts. This ordering is the whole fix, so it is
        // pinned for every shell that gets generated options.
        for path in ["/bin/bash", "/usr/bin/fish", "/bin/zsh"] {
            let dir = scratch("order");
            let inv = Shell::for_path(path.into()).invocation(&dir);
            let interactive = inv.args.iter().position(|a| a == "-i").expect("-i");
            for (i, arg) in inv.args.iter().enumerate() {
                if arg.starts_with("--") {
                    let message = format!("{path}: long option {arg} came after -i: {:?}", inv.args);
                    assert!(i < interactive, "{message}");
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn fish_gets_an_init_command_with_the_hooks() {
        let inv = Shell::for_path("/usr/bin/fish".into()).invocation(Path::new("/tmp"));
        assert_eq!(inv.args.len(), 2);
        assert!(inv.args[0].starts_with("--init-command="));
        assert_eq!(inv.args[1], "-i");
        assert!(inv.args[0].contains("fish_preexec"));
        assert!(inv.args[0].contains("133"));
        assert!(!inv.args[0].contains('\n'), "--init-command takes one command string");
    }

    #[test]
    fn a_non_interactive_request_is_left_alone() {
        // `eye record -- python` style: never inject flags into a program the
        // user named with a specific mode.
        let mut shell = Shell::for_path("/bin/bash".into());
        shell.lead_args = vec!["-c".into(), "echo hi".into()];
        let inv = shell.invocation(Path::new("/tmp"));
        assert_eq!(inv.args, vec!["-c".to_owned(), "echo hi".to_owned()]);
        assert!(inv.env.is_empty());
    }

    #[test]
    fn eye_variables_never_leak_into_the_child() {
        unsafe {
            std::env::set_var("EYE_SHOULD_BE_STRIPPED", "1");
            std::env::set_var("ZDOTDIR", "/nope");
        }
        let env = Shell::for_path("/bin/sh".into())
            .env(&[("EYE_TEST_KEEP".into(), "1".into())]);
        assert!(!env.iter().any(|(k, _)| k == "EYE_SHOULD_BE_STRIPPED"));
        assert!(!env.iter().any(|(k, _)| k == "ZDOTDIR"));
        assert!(env.iter().any(|(k, v)| k == "EYE_TEST_KEEP" && v == "1"));
        unsafe {
            std::env::remove_var("EYE_SHOULD_BE_STRIPPED");
            std::env::remove_var("EYE_TEST_KEEP");
        }
    }

    #[test]
    fn hooks_contain_the_marks_we_parse() {
        for script in [bash::HOOKS, zsh::HOOKS, fish::HOOKS] {
            assert!(script.contains("__eye_mark"), "hooks must route marks through one helper");
            // bash spells the mark out; zsh and fish pass `D;<status>` to the
            // helper, so both shapes have to be accepted.
            assert!(
                script.contains("133;D") || script.contains("\"D;"),
                "missing exit-status mark in:\n{script}"
            );
            assert!(script.contains("133;C") || script.contains(" C"), "missing command-start mark");
        }
    }
}
