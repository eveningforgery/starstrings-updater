//! Command-line parsing and the `run` / `status` commands.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use crate::config::{self, Instance};
use crate::detect;
use crate::install::{self, EXIT_FAILED, EXIT_OK, EXIT_USAGE, InstallOptions, UninstallOptions};
use crate::logger;
use crate::state::State;
use crate::task;
use crate::ui;
use crate::update::{self, USER_CFG};
use crate::usercfg;
use crate::win::InstanceLock;

const HELP: &str = "\
Keeps MrKraken's StarStrings up to date in your Star Citizen folder.

Usage:
  starstrings-updater                  Install (or, if installed, update and show the status)

  starstrings-updater install [options]
      --path <folder>    Star Citizen folder, if it is not found automatically
      --ptu              Also keep the PTU folder up to date
      --live-only        Stop keeping PTU up to date
      --quiet            Show no dialogs

  starstrings-updater uninstall [options]
      --remove-strings   Also remove the StarStrings files from the game
      --quiet            Show no dialogs

  starstrings-updater run [--path <folder>] [--ptu]
                                       Check for a new release now. This is what the
                                       scheduled task runs; it never shows a window.

  starstrings-updater status           Show what is installed
  starstrings-updater --version
  starstrings-updater --help";

/// Exit code for a bug in the program itself.
const EXIT_INTERNAL: i32 = 70;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub path: Option<PathBuf>,
    pub ptu: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Interactive,
    Install(InstallOptions),
    Uninstall(UninstallOptions),
    Run(RunOptions),
    Status,
    Help,
    Version,
}

/// Collects `--flag`, `--key value` and `--key=value` arguments.
struct Flags {
    seen: Vec<(String, Option<String>)>,
}

impl Flags {
    fn parse(args: &[String], with_value: &[&str], switches: &[&str]) -> Result<Flags, String> {
        let mut seen = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if with_value.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        args.get(index)
                            .cloned()
                            .ok_or_else(|| format!("{name} needs a value"))?
                    }
                };
                if value.is_empty() {
                    return Err(format!("{name} needs a value"));
                }
                seen.push((name.to_string(), Some(value)));
            } else if switches.contains(&name) && inline.is_none() {
                seen.push((name.to_string(), None));
            } else {
                return Err(format!("unknown option: {arg}"));
            }
            index += 1;
        }
        Ok(Flags { seen })
    }

    fn has(&self, names: &[&str]) -> bool {
        self.seen
            .iter()
            .any(|(name, _)| names.contains(&name.as_str()))
    }

    fn value(&self, name: &str) -> Option<String> {
        self.seen
            .iter()
            .rev()
            .find(|(seen, _)| seen == name)
            .and_then(|(_, value)| value.clone())
    }
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Ok(Command::Interactive);
    };
    let rest = &args[1..];
    let no_arguments = |command: Command| {
        if rest.is_empty() {
            Ok(command)
        } else {
            Err(format!("unexpected argument: {}", rest[0]))
        }
    };
    match first.as_str() {
        "install" => {
            let flags = Flags::parse(
                rest,
                &["--path"],
                &["--ptu", "--live-only", "--quiet", "-q"],
            )?;
            if flags.has(&["--ptu"]) && flags.has(&["--live-only"]) {
                return Err("--ptu and --live-only cannot be combined".to_string());
            }
            Ok(Command::Install(InstallOptions {
                path: flags.value("--path").map(PathBuf::from),
                ptu: flags.has(&["--ptu"]),
                live_only: flags.has(&["--live-only"]),
                quiet: flags.has(&["--quiet", "-q"]),
            }))
        }
        "uninstall" => {
            let flags = Flags::parse(rest, &[], &["--remove-strings", "--quiet", "-q"])?;
            Ok(Command::Uninstall(UninstallOptions {
                remove_strings: flags.has(&["--remove-strings"]),
                quiet: flags.has(&["--quiet", "-q"]),
            }))
        }
        "run" => {
            let flags = Flags::parse(rest, &["--path"], &["--ptu"])?;
            Ok(Command::Run(RunOptions {
                path: flags.value("--path").map(PathBuf::from),
                ptu: flags.has(&["--ptu"]),
            }))
        }
        "status" => no_arguments(Command::Status),
        "help" | "--help" | "-h" | "/?" => no_arguments(Command::Help),
        "version" | "--version" | "-V" => no_arguments(Command::Version),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Program entry point; returns the process exit code.
pub fn main() -> i32 {
    ui::attach_parent_console();
    let inst = Instance::current();
    logger::init(inst.log_path());
    // A bug must not turn into a crash dialog on someone's desktop: record it
    // and leave quietly with a failure code.
    std::panic::set_hook(Box::new(|info| {
        logger::error(&format!("internal error: {info}"));
        std::process::exit(EXIT_INTERNAL);
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let title = inst.display_name();
    let heading = format!("{} {}", config::APP_NAME, config::VERSION);
    match parse(&args) {
        Ok(Command::Interactive) => install::interactive(&inst),
        Ok(Command::Install(options)) => install::install(&inst, &options, false),
        Ok(Command::Uninstall(options)) => install::uninstall(&inst, &options),
        Ok(Command::Run(options)) => run(&inst, &options),
        Ok(Command::Status) => {
            ui::show(&title, &status(&inst));
            EXIT_OK
        }
        Ok(Command::Help) => {
            ui::show(&title, &format!("{heading}\n{HELP}"));
            EXIT_OK
        }
        Ok(Command::Version) => {
            ui::show(&title, &heading);
            EXIT_OK
        }
        Err(message) => {
            ui::show(&title, &format!("{message}\n\n{HELP}"));
            EXIT_USAGE
        }
    }
}

/// The scheduled check. Never shows a window; the exit code and the log file
/// are the only traces it leaves.
fn run(inst: &Instance, options: &RunOptions) -> i32 {
    let Some(_lock) = InstanceLock::acquire(&inst.lock_name(), 0) else {
        logger::say("Another copy of StarStrings Updater is already working; nothing to do.");
        return EXIT_OK;
    };
    let mut state = match State::load(&inst.state_path()) {
        Ok(state) => state.unwrap_or_default(),
        Err(e) => {
            logger::error(&format!(
                "could not read {}: {e}",
                inst.state_path().display()
            ));
            return EXIT_FAILED;
        }
    };
    let before = state.clone();

    if let Some(path) = &options.path {
        match detect::normalize_root(path) {
            Some(root) => state.game_root = Some(root),
            None => {
                logger::error(&format!(
                    "{} does not look like a Star Citizen folder; use the folder that contains LIVE",
                    path.display()
                ));
                return EXIT_USAGE;
            }
        }
    }
    if !state.channels.iter().any(|c| c == config::LIVE) {
        state.channels.insert(0, config::LIVE.to_string());
    }
    if options.ptu {
        state.enable(config::PTU);
    }

    let mut code = match update::run(inst, &mut state, false) {
        Ok(summary) if summary.succeeded() => EXIT_OK,
        Ok(_) => EXIT_FAILED,
        Err(message) => {
            logger::error(&message);
            EXIT_FAILED
        }
    };
    // Only touch the disk when something actually changed.
    if state != before
        && let Err(e) = state.save(&inst.state_path())
    {
        logger::error(&format!(
            "could not save {}: {e}",
            inst.state_path().display()
        ));
        code = EXIT_FAILED;
    }
    code
}

fn status(inst: &Instance) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} {}", config::APP_NAME, config::VERSION);
    let _ = writeln!(out, "Data folder: {}", inst.home.display());

    let state = State::load(&inst.state_path()).ok().flatten();
    let Some(state) = state.filter(|s| s.game_root.is_some()) else {
        let _ = writeln!(
            out,
            "Not installed. Run \"starstrings-updater install\" to set it up."
        );
        // Looking is harmless, and it answers "will it find my game?" up front.
        match detect::detect() {
            Some(root) => {
                let _ = write!(out, "Star Citizen found at: {}", root.display());
            }
            None => {
                let _ = write!(
                    out,
                    "Star Citizen was not found automatically; use \"install --path <folder>\"."
                );
            }
        }
        return out;
    };
    let root = state.game_root.clone().unwrap_or_default();
    let _ = writeln!(out, "Game folder: {}", root.display());

    match task::query(&inst.task_name()) {
        Some(info) => {
            let _ = writeln!(
                out,
                "Scheduled task: {}",
                if info.enabled {
                    "registered (at sign-in and every hour)"
                } else {
                    "registered but DISABLED"
                }
            );
            match info.last_run {
                Some(when) => {
                    let outcome = if info.last_result == 0 {
                        "succeeded".to_string()
                    } else {
                        format!("exit code {}", info.last_result)
                    };
                    let _ = writeln!(out, "  last run: {when} ({outcome})");
                }
                None => {
                    let _ = writeln!(out, "  last run: not yet");
                }
            }
            if let Some(when) = info.next_run {
                let _ = writeln!(out, "  next run: {when}");
            }
        }
        None => {
            let _ = writeln!(
                out,
                "Scheduled task: NOT registered - run \"starstrings-updater install\" to repair"
            );
        }
    }

    for name in &state.channels {
        let dir = root.join(name);
        let _ = writeln!(out);
        if !detect::is_channel_dir(&dir) {
            let _ = writeln!(out, "{name}: not installed in the game folder");
            continue;
        }
        let Some(cs) = state
            .per_channel
            .get(name)
            .filter(|cs| !cs.files.is_empty())
        else {
            let _ = writeln!(out, "{name}: StarStrings not installed yet");
            continue;
        };
        let release = cs
            .last_modified
            .as_deref()
            .map(update::short_date)
            .unwrap_or_else(|| "unknown date".to_string());
        let installed = cs.updated_at.as_deref().unwrap_or("unknown");
        let _ = writeln!(
            out,
            "{name}: StarStrings release of {release}, installed {installed}"
        );
        for file in &cs.files {
            let path = dir.join(&file.rel);
            let condition = if update::file_matches(&path, file.size, file.crc32) {
                "ok"
            } else if path.is_file() {
                "changed since it was installed"
            } else {
                "MISSING (will be restored at the next check)"
            };
            let _ = writeln!(out, "  {} ({} bytes): {condition}", file.rel, file.size);
        }
        let language = fs::read(dir.join(USER_CFG))
            .ok()
            .and_then(|content| usercfg::current_language(&content));
        match language {
            Some(language) if language.eq_ignore_ascii_case(usercfg::LANGUAGE) => {
                let _ = writeln!(out, "  {USER_CFG}: g_language = {language}");
            }
            Some(language) => {
                let _ = writeln!(
                    out,
                    "  {USER_CFG}: g_language = {language} - StarStrings is not active with this setting"
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  {USER_CFG}: g_language is not set (will be added at the next check)"
                );
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_interactive() {
        assert_eq!(parse(&[]), Ok(Command::Interactive));
    }

    #[test]
    fn parses_install() {
        assert_eq!(
            parse(&args(&["install"])),
            Ok(Command::Install(InstallOptions::default()))
        );
        assert_eq!(
            parse(&args(&[
                "install",
                "--path",
                r"D:\Games\StarCitizen",
                "--ptu",
                "--quiet"
            ])),
            Ok(Command::Install(InstallOptions {
                path: Some(PathBuf::from(r"D:\Games\StarCitizen")),
                ptu: true,
                live_only: false,
                quiet: true,
            }))
        );
        assert_eq!(
            parse(&args(&[
                "install",
                r"--path=C:\Program Files\X",
                "-q",
                "--live-only"
            ])),
            Ok(Command::Install(InstallOptions {
                path: Some(PathBuf::from(r"C:\Program Files\X")),
                ptu: false,
                live_only: true,
                quiet: true,
            }))
        );
    }

    #[test]
    fn parses_the_other_commands() {
        assert_eq!(
            parse(&args(&["uninstall", "--quiet", "--remove-strings"])),
            Ok(Command::Uninstall(UninstallOptions {
                remove_strings: true,
                quiet: true,
            }))
        );
        assert_eq!(
            parse(&args(&["run"])),
            Ok(Command::Run(RunOptions::default()))
        );
        assert_eq!(
            parse(&args(&["run", "--path", "X", "--ptu"])),
            Ok(Command::Run(RunOptions {
                path: Some(PathBuf::from("X")),
                ptu: true,
            }))
        );
        assert_eq!(parse(&args(&["status"])), Ok(Command::Status));
        for help in ["help", "--help", "-h", "/?"] {
            assert_eq!(parse(&args(&[help])), Ok(Command::Help));
        }
        for version in ["version", "--version", "-V"] {
            assert_eq!(parse(&args(&[version])), Ok(Command::Version));
        }
    }

    #[test]
    fn rejects_mistakes() {
        for bad in [
            vec!["frobnicate"],
            vec!["install", "--path"],
            vec!["install", "--path="],
            vec!["install", "--nope"],
            vec!["install", "--quiet=yes"],
            vec!["install", "--ptu", "--live-only"],
            vec!["install", "stray"],
            vec!["uninstall", "--path", "x"],
            vec!["run", "--quiet"],
            vec!["status", "extra"],
            vec!["--help", "extra"],
            vec!["--run"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }
}
