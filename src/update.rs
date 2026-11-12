//! The actual work: ask whether the release changed, install it if so, and
//! keep `USER.cfg` pointing at it.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::archive;
use crate::config::{self, Channel, Instance};
use crate::detect;
use crate::http::{self, Conditions, Fetched, HttpError};
use crate::logger;
use crate::state::{ChannelState, InstalledFile, State};
use crate::usercfg::{self, Change};
use crate::win;

pub const USER_CFG: &str = "USER.cfg";

/// If the server ever stops sending validators, fall back to one full download
/// a day rather than one an hour.
const REFETCH_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do: the installed release is the current one.
    UpToDate,
    /// New files were written to the game folder.
    Updated { written: usize },
    /// The channel is not installed on this machine.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub cfg: Change,
    /// A problem that did not prevent the update itself (USER.cfg not editable).
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub offline: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Failure {
        Failure {
            message: message.into(),
            offline: false,
        }
    }
}

impl From<HttpError> for Failure {
    fn from(e: HttpError) -> Failure {
        Failure {
            message: e.message,
            offline: e.offline,
        }
    }
}

pub struct ChannelResult {
    pub channel: &'static str,
    pub result: Result<Report, Failure>,
    /// `Last-Modified` of the release that is installed after this run.
    pub release: Option<String>,
}

/// `Thu, 12 Nov 2026 17:31:49 GMT` -> `12 Nov 2026`.
pub fn short_date(http_date: &str) -> String {
    let parts: Vec<&str> = http_date.split_whitespace().collect();
    match parts.as_slice() {
        [_, day, month, year, _, _] if day.parse::<u8>().is_ok() && year.parse::<u16>().is_ok() => {
            format!("{} {month} {year}", day.trim_start_matches('0'))
        }
        _ => http_date.to_string(),
    }
}

impl ChannelResult {
    fn release_text(&self) -> String {
        self.release
            .as_deref()
            .map(|date| format!(" (release of {})", short_date(date)))
            .unwrap_or_default()
    }

    /// One line saying what happened to this channel.
    pub fn headline(&self) -> String {
        let name = self.channel;
        match &self.result {
            Ok(report) => match report.outcome {
                Outcome::Updated { .. } => {
                    format!("{name}: StarStrings updated{}", self.release_text())
                }
                Outcome::UpToDate => {
                    format!("{name}: StarStrings is up to date{}", self.release_text())
                }
                Outcome::Skipped => format!("{name}: not installed in the game folder, skipped"),
            },
            Err(failure) => format!("{name}: {}", failure.message),
        }
    }

    /// What was done to USER.cfg, if anything.
    fn cfg_line(&self) -> Option<String> {
        let name = self.channel;
        match &self.result.as_ref().ok()?.cfg {
            Change::Unchanged => None,
            Change::Added => Some(format!(
                "{name}: added \"g_language = {}\" to {USER_CFG}",
                usercfg::LANGUAGE
            )),
            Change::Replaced { previous } => Some(format!(
                "{name}: changed g_language from \"{previous}\" to \"{}\" in {USER_CFG}",
                usercfg::LANGUAGE
            )),
        }
    }

    fn warning_line(&self) -> Option<String> {
        let warning = self.result.as_ref().ok()?.warning.as_ref()?;
        Some(format!("{}: {warning}", self.channel))
    }

    /// Additional lines: what was done to USER.cfg, and any warning.
    pub fn details(&self) -> Vec<String> {
        self.cfg_line()
            .into_iter()
            .chain(self.warning_line())
            .collect()
    }

    pub fn succeeded(&self) -> bool {
        matches!(&self.result, Ok(report) if report.warning.is_none())
    }

    fn log(&self) {
        match &self.result {
            Ok(report) => {
                if matches!(report.outcome, Outcome::Updated { .. }) {
                    logger::info(&self.headline());
                } else {
                    logger::say(&self.headline());
                }
                if let Some(line) = self.cfg_line() {
                    logger::info(&line);
                }
                if let Some(line) = self.warning_line() {
                    logger::error(&line);
                }
            }
            // Being offline is normal for a laptop; it is not worth a log entry.
            Err(failure) if failure.offline => logger::say(&self.headline()),
            Err(_) => logger::error(&self.headline()),
        }
    }
}

pub struct Summary {
    pub channels: Vec<ChannelResult>,
}

impl Summary {
    pub fn succeeded(&self) -> bool {
        self.channels.iter().all(ChannelResult::succeeded)
    }

    pub fn lines(&self) -> Vec<String> {
        self.channels
            .iter()
            .flat_map(|c| std::iter::once(c.headline()).chain(c.details()))
            .collect()
    }
}

/// Checks every enabled channel and installs whatever changed.
///
/// `force_language` is set by the installer: it switches an existing
/// `g_language` setting to English instead of merely adding a missing one.
pub fn run(inst: &Instance, state: &mut State, force_language: bool) -> Result<Summary, String> {
    let root = resolve_root(state)?;
    if state.channels.is_empty() {
        state.enable(config::LIVE);
    }
    let mut channels = Vec::new();
    for name in state.channels.clone() {
        let Some(channel) = config::channel(&name) else {
            continue;
        };
        let cs = state.channel_mut(channel.name);
        let result = update_channel(inst, &root, channel, cs, force_language);
        let item = ChannelResult {
            channel: channel.name,
            result,
            release: cs.last_modified.clone(),
        };
        item.log();
        channels.push(item);
    }
    Ok(Summary { channels })
}

/// The stored game folder, or a freshly detected one if the game has moved.
fn resolve_root(state: &mut State) -> Result<PathBuf, String> {
    let stored = state.game_root.clone();
    if let Some(root) = &stored
        && detect::is_game_root(root)
    {
        return Ok(root.clone());
    }
    // No game where it used to be: the library may have been moved.
    if let Some(found) = detect::detect() {
        if stored.as_ref() != Some(&found) {
            logger::info(&format!("Star Citizen folder: {}", found.display()));
            state.game_root = Some(found.clone());
        }
        return Ok(found);
    }
    // Not found anywhere. If the old folder still exists the game is probably
    // being reinstalled; keep waiting for it rather than reporting an error.
    match stored {
        Some(root) if root.is_dir() => Ok(root),
        _ => Err(
            "the Star Citizen folder could not be found; run \"starstrings-updater install --path <folder>\""
                .to_string(),
        ),
    }
}

fn fetched_recently(cs: &ChannelState) -> bool {
    cs.fetched_at
        .is_some_and(|at| win::unix_now().saturating_sub(at) < REFETCH_INTERVAL_SECS)
}

fn update_channel(
    inst: &Instance,
    root: &Path,
    channel: &Channel,
    cs: &mut ChannelState,
    force_language: bool,
) -> Result<Report, Failure> {
    let dir = root.join(channel.name);
    if !detect::is_channel_dir(&dir) {
        return Ok(Report {
            outcome: Outcome::Skipped,
            cfg: Change::Unchanged,
            warning: None,
        });
    }

    // "Not modified" is only a valid answer while everything installed earlier
    // is still in place. If a file has gone missing (a game repair, a manual
    // clean-up), ask unconditionally so it is put back.
    let intact = !cs.files.is_empty() && cs.files.iter().all(|f| dir.join(&f.rel).is_file());
    let conditions = if intact {
        Conditions {
            etag: cs.etag.as_deref(),
            last_modified: cs.last_modified.as_deref(),
        }
    } else {
        Conditions::default()
    };

    let outcome = if intact && conditions.is_empty() && fetched_recently(cs) {
        Outcome::UpToDate
    } else {
        let url = config::source_url(channel);
        match http::fetch(&url, &conditions, config::MAX_ARCHIVE_BYTES)? {
            Fetched::NotModified => Outcome::UpToDate,
            Fetched::Body {
                data,
                etag,
                last_modified,
            } => {
                let written = install_archive(inst, channel, &dir, &data, cs)?;
                cs.etag = etag;
                cs.last_modified = last_modified;
                cs.fetched_at = Some(win::unix_now());
                if written > 0 {
                    cs.updated_at = Some(win::local_timestamp());
                    Outcome::Updated { written }
                } else {
                    Outcome::UpToDate
                }
            }
        }
    };

    let (cfg, warning) = match ensure_user_cfg(&dir, cs, force_language) {
        Ok(change) => (change, None),
        Err(message) => (Change::Unchanged, Some(message)),
    };
    Ok(Report {
        outcome,
        cfg,
        warning,
    })
}

/// Installs the localization files from a downloaded release archive.
///
/// Returns how many files were written; files already identical on disk are
/// left alone. Nothing is written unless every file in the archive checks out.
fn install_archive(
    inst: &Instance,
    channel: &Channel,
    dir: &Path,
    zip: &[u8],
    cs: &mut ChannelState,
) -> Result<usize, Failure> {
    let entries = archive::entries(zip).map_err(Failure::new)?;

    let mut wanted: Vec<(String, &archive::Entry)> = Vec::new();
    for entry in &entries {
        let Some(rel) = archive::install_path(&entry.name) else {
            continue;
        };
        if wanted
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(&rel))
        {
            return Err(Failure::new(format!(
                "the archive lists {rel} more than once; not installing it"
            )));
        }
        wanted.push((rel, entry));
    }
    if wanted.is_empty() {
        return Err(Failure::new(
            "the release archive contains no localization files; its layout may have changed",
        ));
    }

    // First decode and verify everything in memory...
    let mut installed = Vec::new();
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    for (rel, entry) in &wanted {
        if entry.size == 0 {
            return Err(Failure::new(format!(
                "{rel} is empty in the release archive; not installing it"
            )));
        }
        installed.push(InstalledFile {
            rel: rel.clone(),
            size: entry.size,
            crc32: entry.crc32,
        });
        if file_matches(&dir.join(rel), entry.size, entry.crc32) {
            continue;
        }
        let data = archive::read(zip, entry, config::MAX_FILE_BYTES).map_err(Failure::new)?;
        pending.push((rel.clone(), data));
    }

    // ...and only then touch the game folder.
    for (rel, data) in &pending {
        let dest = dir.join(rel);
        let ours = cs.files.iter().any(|f| f.rel.eq_ignore_ascii_case(rel));
        if !ours && dest.is_file() {
            back_up(inst, channel, rel, &dest);
        }
        write_atomic(&dest, data).map_err(|e| {
            Failure::new(format!(
                "could not write {} ({e}); it will be retried on the next check",
                dest.display()
            ))
        })?;
    }

    // Remove files from an earlier release that are no longer shipped, but only
    // if they are still exactly what was installed.
    for old in &cs.files {
        let still_shipped = installed
            .iter()
            .any(|f| f.rel.eq_ignore_ascii_case(&old.rel));
        let path = dir.join(&old.rel);
        if !still_shipped && file_matches(&path, old.size, old.crc32) {
            let _ = fs::remove_file(&path);
        }
    }

    cs.files = installed;
    Ok(pending.len())
}

/// Keeps a one-time copy of a file that was there before the updater first
/// replaced it, in case it was something the player made themselves.
fn back_up(inst: &Instance, channel: &Channel, rel: &str, existing: &Path) {
    let target = inst.backup_dir().join(channel.name).join(rel);
    if target.exists() {
        return;
    }
    let copied = target
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|_| fs::copy(existing, &target).map(|_| ()));
    match copied {
        Ok(()) => logger::info(&format!(
            "{}: kept a copy of the previous {rel} in {}",
            channel.name,
            target.display()
        )),
        Err(e) => logger::error(&format!(
            "{}: could not keep a copy of the previous {rel}: {e}",
            channel.name
        )),
    }
}

fn ensure_user_cfg(dir: &Path, cs: &mut ChannelState, force: bool) -> Result<Change, String> {
    let path = dir.join(USER_CFG);
    let existing = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not read {USER_CFG}: {e}")),
    };
    let (updated, change) = usercfg::ensure(existing.as_deref().unwrap_or_default(), force)?;
    match &change {
        Change::Unchanged => return Ok(change),
        Change::Added => {
            cs.cfg_added = true;
            cs.cfg_created = existing.is_none();
        }
        Change::Replaced { previous } => {
            // Remember only the value from before the updater was involved.
            if !cs.cfg_added && cs.cfg_previous.is_none() {
                cs.cfg_previous = Some(previous.clone());
            }
        }
    }
    write_atomic(&path, &updated).map_err(|e| format!("could not update {USER_CFG}: {e}"))?;
    Ok(change)
}

fn crc32_of_file(path: &Path) -> io::Result<u32> {
    let mut file = File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

/// Whether `path` is a file with exactly this size and content checksum.
pub fn file_matches(path: &Path, size: u64, crc32: u32) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
        && crc32_of_file(path).is_ok_and(|actual| actual == crc32)
}

/// Replaces `dest` with `data` in one step: the content is written to a
/// temporary file beside it and then renamed over it, so the game can never
/// observe a half-written file.
pub fn write_atomic(dest: &Path, data: &[u8]) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid destination path");
    let dir = dest.parent().ok_or_else(invalid)?;
    let name = dest.file_name().ok_or_else(invalid)?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.ssu-tmp", name.to_string_lossy()));

    let written = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        replace(&tmp, dest)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn set_read_only(path: &Path, read_only: bool) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(read_only);
    fs::set_permissions(path, permissions)
}

fn replace(tmp: &Path, dest: &Path) -> io::Result<()> {
    let Err(error) = fs::rename(tmp, dest) else {
        return Ok(());
    };
    // Some players mark these files read-only. Lift the attribute for the swap
    // and put it back afterwards, so the file keeps the protection they chose.
    let read_only = fs::metadata(dest).is_ok_and(|m| m.permissions().readonly());
    if !read_only {
        return Err(error);
    }
    set_read_only(dest, false)?;
    let renamed = fs::rename(tmp, dest);
    let _ = set_read_only(dest, true);
    renamed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_http_dates() {
        assert_eq!(short_date("Thu, 12 Nov 2026 17:31:49 GMT"), "12 Nov 2026");
        assert_eq!(short_date("Mon, 09 Nov 2026 23:52:58 GMT"), "9 Nov 2026");
        assert_eq!(short_date("yesterday"), "yesterday");
        assert_eq!(short_date(""), "");
    }

    #[test]
    fn write_atomic_creates_folders_and_replaces_content() {
        let dir = std::env::temp_dir().join(format!("ssu-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dest = dir.join("a").join("b").join("file.ini");

        write_atomic(&dest, b"one").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"one");
        write_atomic(&dest, b"two!").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"two!");
        assert!(!dest.with_file_name("file.ini.ssu-tmp").exists());

        assert!(file_matches(&dest, 4, crc32fast::hash(b"two!")));
        assert!(!file_matches(&dest, 3, crc32fast::hash(b"two!")));
        assert!(!file_matches(&dest, 4, crc32fast::hash(b"tw0!")));
        assert!(!file_matches(&dir.join("missing"), 0, 0));
        assert!(!file_matches(&dir, 0, 0));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_replaces_read_only_files_and_keeps_them_read_only() {
        let dir = std::env::temp_dir().join(format!("ssu-readonly-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dest = dir.join("USER.cfg");
        write_atomic(&dest, b"old").unwrap();
        set_read_only(&dest, true).unwrap();

        write_atomic(&dest, b"new").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(fs::metadata(&dest).unwrap().permissions().readonly());

        set_read_only(&dest, false).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
