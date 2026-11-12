//! Installing, repairing and removing the updater itself.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::config::{self, Instance};
use crate::detect;
use crate::logger;
use crate::state::{ChannelState, State};
use crate::task;
use crate::ui;
use crate::update::{self, USER_CFG};
use crate::usercfg;
use crate::win::{self, InstanceLock, RegValue};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CANCELLED: i32 = 3;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    pub path: Option<PathBuf>,
    pub ptu: bool,
    pub live_only: bool,
    pub quiet: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UninstallOptions {
    pub remove_strings: bool,
    pub quiet: bool,
}

const REMOVAL_HINT: &str =
    "To remove it: Windows Settings > Apps > Installed apps > StarStrings Updater.";

/// Whether the updater is fully set up on this machine.
pub fn is_installed(inst: &Instance) -> bool {
    let has_state = State::load(&inst.state_path())
        .ok()
        .flatten()
        .is_some_and(|s| s.game_root.is_some());
    has_state && inst.installed_exe().is_file() && task::query(&inst.task_name()).is_some()
}

/// Started without arguments (a double-click): install after asking, or - if
/// already installed - bring everything up to date and show the status.
pub fn interactive(inst: &Instance) -> i32 {
    let ask_first = !is_installed(inst);
    install(inst, &InstallOptions::default(), ask_first)
}

enum Located {
    Found(PathBuf),
    Cancelled,
    Missing(String),
}

fn locate_game(options: &InstallOptions, state: &State, title: &str) -> Located {
    if let Some(path) = &options.path {
        return match detect::normalize_root(path) {
            Some(root) => Located::Found(root),
            None => Located::Missing(format!(
                "{} does not look like a Star Citizen folder. Use the folder that contains LIVE.",
                path.display()
            )),
        };
    }
    if let Some(root) = &state.game_root
        && detect::is_game_root(root)
    {
        return Located::Found(root.clone());
    }
    if let Some(root) = detect::detect() {
        return Located::Found(root);
    }
    if options.quiet {
        return Located::Missing(
            "Star Citizen was not found. Run again with --path \"<your StarCitizen folder>\"."
                .to_string(),
        );
    }
    ui::info(
        title,
        "Star Citizen was not found automatically.\n\nIn the next window, select your StarCitizen \
         folder - the one that contains the LIVE folder.",
    );
    loop {
        let Some(picked) =
            ui::pick_folder("Select your StarCitizen folder (the one that contains LIVE)")
        else {
            return Located::Cancelled;
        };
        if let Some(root) = detect::normalize_root(&picked) {
            return Located::Found(root);
        }
        let retry = ui::confirm(
            title,
            &format!(
                "{}\n\ndoes not contain a Star Citizen installation.\n\nChoose another folder?",
                picked.display()
            ),
        );
        if !retry {
            return Located::Cancelled;
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
}

/// Puts a copy of this program where the scheduled task will run it from.
///
/// The bytes are written to a new file rather than copied, so the installed
/// program is an ordinary local file like any other installed application.
fn place_executable(target: &Path) -> io::Result<()> {
    let current = env::current_exe()?;
    if same_file(&current, target) {
        return Ok(());
    }
    let bytes = fs::read(&current)?;
    if fs::read(target).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    // The scheduled task may be running the old copy at this very moment; it
    // only lives for a second, so a short wait is enough.
    let mut attempt = 0;
    loop {
        match update::write_atomic(target, &bytes) {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 20 => return Err(e),
            Err(_) => {
                attempt += 1;
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn register_app(inst: &Instance, exe: &Path) -> Result<(), String> {
    let exe_text = exe.display().to_string();
    let home = inst.home.display().to_string();
    let uninstall = format!("\"{exe_text}\" uninstall");
    let quiet_uninstall = format!("\"{exe_text}\" uninstall --quiet");
    let size_kb = fs::metadata(exe)
        .map(|m| (m.len() / 1024) as u32)
        .unwrap_or(0);
    win::write_uninstall_entry(
        &inst.registry_key(),
        &[
            ("DisplayName", RegValue::Text(&inst.display_name())),
            ("DisplayVersion", RegValue::Text(config::VERSION)),
            ("Publisher", RegValue::Text(config::PUBLISHER)),
            ("DisplayIcon", RegValue::Text(&exe_text)),
            ("InstallLocation", RegValue::Text(&home)),
            ("UninstallString", RegValue::Text(&uninstall)),
            ("QuietUninstallString", RegValue::Text(&quiet_uninstall)),
            ("URLInfoAbout", RegValue::Text(config::PROJECT_URL)),
            ("NoModify", RegValue::Number(1)),
            ("NoRepair", RegValue::Number(1)),
            ("EstimatedSize", RegValue::Number(size_kb)),
        ],
    )
}

fn consent_text(root: &Path, ptu: bool) -> String {
    let folders = if ptu {
        "LIVE and PTU folders"
    } else {
        "LIVE folder"
    };
    format!(
        "Install StarStrings Updater?\n\n\
         Star Citizen folder:\n{}\n\n\
         It will install MrKraken's StarStrings into the {folders} now, and then keep it up to \
         date by itself: a quick check when you sign in and once an hour. Nothing keeps running \
         in the background between checks.\n\n\
         You can remove it at any time from Windows Settings > Apps > Installed apps.",
        root.display()
    )
}

const APP_PRIVATE_MESSAGE: &str = "StarStrings Updater cannot be installed from here.\n\n\
It was started from inside another app that keeps the files it creates private to itself (a \
packaged app, such as one from the Microsoft Store). Windows would not be able to find the \
installed program, so the automatic updates would never run.\n\n\
Start starstrings-updater.exe directly from File Explorer instead.";

/// Whether this process would install into a folder only it (and the app it
/// was started from) can see. See `win::is_app_private`.
///
/// Checked before anything is installed: a scheduled task pointing into such a
/// folder can never start, so the installation would look fine and do nothing.
fn home_is_app_private(inst: &Instance) -> bool {
    let existed = inst.home.exists();
    if fs::create_dir_all(&inst.home).is_err() {
        return false;
    }
    let private = fs::canonicalize(&inst.home)
        .is_ok_and(|resolved| win::is_app_private(&inst.home, &resolved));
    if !existed {
        // Leave no trace of the probe; the installer creates the folder again.
        let _ = fs::remove_dir(&inst.home);
    }
    private
}

/// Installs or repairs the updater, then runs the first update.
pub fn install(inst: &Instance, options: &InstallOptions, ask_first: bool) -> i32 {
    let title = inst.display_name();
    let quiet = options.quiet;
    let fail = |message: &str| -> i32 {
        logger::error(message);
        if !quiet {
            ui::error(&title, message);
        }
        EXIT_FAILED
    };

    if home_is_app_private(inst) {
        // Not recorded in the log file: that would itself end up in the private folder.
        logger::say(APP_PRIVATE_MESSAGE);
        if !quiet {
            ui::error(&title, APP_PRIVATE_MESSAGE);
        }
        return EXIT_FAILED;
    }

    let Some(_lock) = InstanceLock::acquire(&inst.lock_name(), 60_000) else {
        return fail("Another copy of StarStrings Updater is busy. Please try again in a moment.");
    };
    let mut state = match State::load(&inst.state_path()) {
        Ok(state) => state.unwrap_or_default(),
        Err(e) => {
            return fail(&format!(
                "Could not read {}: {e}",
                inst.state_path().display()
            ));
        }
    };

    let root = match locate_game(options, &state, &title) {
        Located::Found(root) => root,
        Located::Cancelled => return EXIT_CANCELLED,
        Located::Missing(message) => return fail(&message),
    };

    if options.live_only {
        state.channels.clear();
    }
    if !state.channels.iter().any(|c| c == config::LIVE) {
        state.channels.insert(0, config::LIVE.to_string());
    }
    if options.ptu {
        state.enable(config::PTU);
    }
    let ptu = state.channels.iter().any(|c| c == config::PTU);

    if ask_first && !quiet && !ui::confirm(&title, &consent_text(&root, ptu)) {
        return EXIT_CANCELLED;
    }

    let exe = inst.installed_exe();
    // If a first-time installation fails part-way, nothing of it should stay.
    let first_time = !exe.exists() && !inst.state_path().exists();
    let undo = || {
        if first_time {
            for name in [config::EXE_NAME, "state.ini", "state.ini.tmp"] {
                let _ = fs::remove_file(inst.home.join(name));
            }
        }
    };
    if let Err(e) = place_executable(&exe) {
        return fail(&format!(
            "Could not copy the updater to {}: {e}",
            exe.display()
        ));
    }

    state.set_game_root(root.clone());
    if let Err(e) = state.save(&inst.state_path()) {
        undo();
        return fail(&format!("Could not save settings: {e}"));
    }

    // The first scheduled check is one interval from now: the installer does
    // the first update itself, right below.
    let registered = win::current_user_sid().and_then(|sid| {
        let first_check = win::local_timestamp_iso_in(task::INTERVAL_SECONDS);
        task::register(
            &inst.task_name(),
            &task::definition(&exe, &sid, &first_check),
        )
    });
    if let Err(e) = registered {
        undo();
        return fail(&format!(
            "Could not create the scheduled task that keeps StarStrings up to date: {e}"
        ));
    }
    if let Err(e) = register_app(inst, &exe) {
        // Not fatal: only the entry in Windows' list of installed apps is missing.
        logger::error(&format!("Could not add the entry to Installed apps: {e}"));
    }
    logger::info(&format!(
        "Installed {} {} for {}",
        config::APP_NAME,
        config::VERSION,
        root.display()
    ));

    // The first update. Failing here (say, no network right now) is not fatal:
    // the scheduled task will simply try again.
    let summary = update::run(inst, &mut state, true);
    if let Err(e) = state.save(&inst.state_path()) {
        logger::error(&format!("Could not save settings: {e}"));
    }

    let (updated, lines) = match &summary {
        Ok(summary) => (summary.succeeded(), summary.lines().join("\n")),
        Err(message) => (false, message.clone()),
    };
    if !quiet {
        if updated {
            ui::info(
                &title,
                &format!(
                    "StarStrings Updater is installed.\n\n{lines}\n\n\
                     It now checks for a new release when you sign in and once an hour; nothing \
                     runs in the background in between.\n\n{REMOVAL_HINT}"
                ),
            );
        } else {
            ui::warn(
                &title,
                &format!(
                    "StarStrings Updater is installed, but the first update did not succeed:\n\n\
                     {lines}\n\nIt will try again by itself within the hour.\n\n{REMOVAL_HINT}"
                ),
            );
        }
    }
    if updated { EXIT_OK } else { EXIT_FAILED }
}

/// Puts the game folder back the way it was before StarStrings was installed
/// into this channel. Returns notes about anything deliberately left alone.
fn remove_strings(inst: &Instance, root: &Path, channel: &str, cs: &ChannelState) -> Vec<String> {
    let dir = root.join(channel);
    let mut notes = Vec::new();

    for file in &cs.files {
        let path = dir.join(&file.rel);
        let backup = inst.backup_dir_for(root).join(channel).join(&file.rel);
        if !update::file_matches(&path, file.size, file.crc32) {
            if path.exists() {
                notes.push(format!(
                    "{channel}: {} was changed after it was installed and has been left in place",
                    file.rel
                ));
            }
            continue;
        }
        if backup.is_file() {
            // There was a different file here before the updater; put it back.
            // The kept copy is only let go of once that has really happened.
            match fs::read(&backup).and_then(|previous| update::write_atomic(&path, &previous)) {
                Ok(()) => {
                    let _ = fs::remove_file(&backup);
                }
                Err(e) => notes.push(format!("{channel}: could not restore {}: {e}", file.rel)),
            }
            continue;
        }
        if let Err(e) = fs::remove_file(&path) {
            notes.push(format!("{channel}: could not remove {}: {e}", file.rel));
            continue;
        }
        // Tidy up folders that only existed for this file, but never above `dir`.
        let mut folder = path.parent();
        while let Some(current) = folder {
            if current == dir || fs::remove_dir(current).is_err() {
                break;
            }
            folder = current.parent();
        }
    }

    if cs.cfg_added || cs.cfg_previous.is_some() {
        let path = dir.join(USER_CFG);
        if let Ok(content) = fs::read(&path) {
            match usercfg::revert(&content, cs.cfg_previous.as_deref()) {
                Ok(reverted) if reverted == content => {}
                Ok(reverted) => {
                    let now_empty = reverted.iter().all(u8::is_ascii_whitespace);
                    let result = if now_empty && cs.cfg_created {
                        fs::remove_file(&path)
                    } else {
                        update::write_in_place(&path, &reverted)
                    };
                    if let Err(e) = result {
                        notes.push(format!("{channel}: could not restore {USER_CFG}: {e}"));
                    }
                }
                Err(e) => notes.push(format!("{channel}: {e}")),
            }
        }
    }
    notes
}

/// Removes `dir` and the folders inside it as far as they are empty.
fn prune_empty_folders(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_empty_folders(&entry.path());
        }
    }
    let _ = fs::remove_dir(dir);
}

/// What the uninstaller had to leave in the updater's own folder.
struct Leftovers {
    /// The program itself, if it could neither be deleted nor moved away.
    program: Option<PathBuf>,
    /// Copies of the player's earlier files that were not put back.
    backups: Option<PathBuf>,
}

/// Deletes what the updater keeps in its own folder.
///
/// Copies of files that were in the game folder before the updater replaced
/// them are the one thing it never deletes: unless they were put back just
/// now, they may be the only copy of something the player made.
fn remove_own_files(inst: &Instance) -> Leftovers {
    for name in [
        "state.ini",
        "state.ini.tmp",
        "updater.log",
        "updater.log.old",
    ] {
        let _ = fs::remove_file(inst.home.join(name));
    }
    prune_empty_folders(&inst.backup_dir());
    let backups = Some(inst.backup_dir()).filter(|dir| dir.is_dir());

    let exe = inst.installed_exe();
    let mut leftover = None;
    if exe.exists() {
        let running_from_there = env::current_exe().is_ok_and(|me| same_file(&me, &exe));
        let mut removed = false;
        if !running_from_there {
            // A scheduled check may be finishing; it is gone within a second.
            for _ in 0..12 {
                if fs::remove_file(&exe).is_ok() {
                    removed = true;
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
        if !removed {
            // Windows will not delete a program while it runs, but it can be
            // moved, so park it in the temporary folder where Windows cleans up.
            let parked = env::temp_dir().join(format!(
                "starstrings-updater-removed-{}.exe",
                std::process::id()
            ));
            if fs::rename(&exe, &parked).is_err() {
                leftover = Some(exe);
            }
        }
    }
    let _ = fs::remove_dir(&inst.home);
    Leftovers {
        program: leftover,
        backups,
    }
}

/// Whether a copy is being kept of a file that was in the game folder before
/// the updater replaced it.
fn has_earlier_files(inst: &Instance, state: &State) -> bool {
    state.game_root.as_ref().is_some_and(|root| {
        state.per_channel.iter().any(|(channel, cs)| {
            cs.files.iter().any(|file| {
                inst.backup_dir_for(root)
                    .join(channel)
                    .join(&file.rel)
                    .is_file()
            })
        })
    })
}

const REMOVE_QUESTION: &str = "Also remove the StarStrings text from the game?\n\n\
Yes - remove it; the game goes back to its original text.\n\
No - keep the StarStrings files that are installed now.";

const RESTORE_QUESTION: &str = "Also undo the changes to your game folder?\n\n\
Yes - put back the files that were there before StarStrings Updater was installed.\n\
No - keep the StarStrings files that are installed now.";

/// Removes the updater. The StarStrings files stay in the game unless asked.
pub fn uninstall(inst: &Instance, options: &UninstallOptions) -> i32 {
    let title = inst.display_name();
    let quiet = options.quiet;

    if !quiet
        && !ui::confirm(
            &title,
            "Remove StarStrings Updater?\n\nStarStrings will no longer be kept up to date \
             automatically.",
        )
    {
        return EXIT_CANCELLED;
    }
    let load = || {
        State::load(&inst.state_path())
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    // Read once to know what to ask; read again below, once nothing else can
    // change it any more.
    let preview = load();
    let has_strings = preview.per_channel.values().any(|cs| !cs.files.is_empty());
    let question = if has_earlier_files(inst, &preview) {
        RESTORE_QUESTION
    } else {
        REMOVE_QUESTION
    };
    let strings_too =
        options.remove_strings || (!quiet && has_strings && ui::ask(&title, question));

    let Some(_lock) = InstanceLock::acquire(&inst.lock_name(), 60_000) else {
        let message = "Another copy of StarStrings Updater is busy. Please try again in a moment.";
        logger::error(message);
        if !quiet {
            ui::error(&title, message);
        }
        return EXIT_FAILED;
    };
    // A scheduled check may have installed a new release while the questions
    // were on screen; what it recorded is what has to be undone.
    let state = load();
    let has_strings = state.per_channel.values().any(|cs| !cs.files.is_empty());

    let mut problems = Vec::new();
    if let Err(e) = task::unregister(&inst.task_name()) {
        problems.push(format!("Could not remove the scheduled task: {e}"));
    }
    win::delete_uninstall_entry(&inst.registry_key());

    if strings_too && let Some(root) = &state.game_root {
        for (channel, cs) in &state.per_channel {
            problems.extend(remove_strings(inst, root, channel, cs));
        }
    }
    let leftovers = remove_own_files(inst);
    if let Some(program) = &leftovers.program {
        problems.push(format!(
            "Could not delete {}; you can delete it yourself.",
            program.display()
        ));
    }

    let mut message = String::from("StarStrings Updater has been removed.");
    if !strings_too && has_strings {
        message.push_str(
            "\n\nThe StarStrings files stay in your game folder, but will no longer be updated.",
        );
    }
    if let Some(backups) = &leftovers.backups {
        message.push_str(&format!(
            "\n\nA copy of what was in your game folder before StarStrings Updater first replaced \
             it has been kept, in case you want it back:\n{}",
            backups.display()
        ));
    }
    for problem in &problems {
        message.push_str("\n\n");
        message.push_str(problem);
    }
    logger::say(&message);
    if !quiet {
        if problems.is_empty() {
            ui::info(&title, &message);
        } else {
            ui::warn(&title, &message);
        }
    }
    if problems.is_empty() {
        EXIT_OK
    } else {
        EXIT_FAILED
    }
}
