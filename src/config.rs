//! Names, file locations and the upstream release endpoints.

use std::env;
use std::path::PathBuf;

use crate::win;

pub const APP_NAME: &str = "StarStrings Updater";
pub const APP_ID: &str = "StarStringsUpdater";
pub const EXE_NAME: &str = "starstrings-updater.exe";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROJECT_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub const PUBLISHER: &str = "starstrings-updater contributors";

/// Sent with every request. Deliberately carries nothing about the machine or user.
pub const USER_AGENT: &str = concat!("starstrings-updater/", env!("CARGO_PKG_VERSION"));

/// Upper bound for a downloaded release archive (the real one is about 2.5 MB).
pub const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// Upper bound for a single extracted file (the real one is about 11 MB).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bounds for one release as a whole. Together with the per-file limit
/// they cap what even a hostile archive can make this program hold in memory
/// or write to the game folder.
pub const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_FILES: usize = 32;

/// How long one download may take from start to finish, in seconds. A server
/// that keeps a connection alive a few bytes at a time must not be able to keep
/// the updater running. `SSU_DOWNLOAD_TIMEOUT` overrides it.
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

pub fn download_timeout() -> std::time::Duration {
    let seconds = env::var("SSU_DOWNLOAD_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| (1..=3600).contains(s))
        .unwrap_or(DOWNLOAD_TIMEOUT_SECS);
    std::time::Duration::from_secs(seconds)
}

/// A Star Citizen release channel and the StarStrings build published for it.
///
/// Upstream republishes each build under a fixed tag and file name, so these
/// URLs always point at the newest release for the channel.
pub struct Channel {
    /// Name of the channel and of its folder inside the game directory.
    pub name: &'static str,
    pub url: &'static str,
    /// Environment variable that overrides `url` (for forks, mirrors and tests).
    pub url_env: &'static str,
}

pub const LIVE: &str = "LIVE";
pub const PTU: &str = "PTU";

pub const CHANNELS: [Channel; 2] = [
    Channel {
        name: LIVE,
        url: "https://github.com/MrKraken/StarStrings/releases/download/latest/StarStrings-LIVE.zip",
        url_env: "SSU_URL_LIVE",
    },
    Channel {
        name: PTU,
        url: "https://github.com/MrKraken/StarStrings/releases/download/latest-ptu/StarStrings-PTU.zip",
        url_env: "SSU_URL_PTU",
    },
];

pub fn channel(name: &str) -> Option<&'static Channel> {
    CHANNELS.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

pub fn source_url(channel: &Channel) -> String {
    env::var(channel.url_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| channel.url.to_string())
}

/// Where this copy of the updater keeps its files and what it calls itself.
///
/// Normally that is `%LOCALAPPDATA%\StarStringsUpdater`. Installing with
/// `SSU_HOME` set selects another folder and, with it, a separate
/// scheduled-task name, registry key and lock, so a development or test copy
/// can never collide with the real installation.
pub struct Instance {
    pub home: PathBuf,
    /// `None` for the standard installation, a short id for any other folder.
    tag: Option<String>,
}

impl Instance {
    pub fn current() -> Instance {
        let standard = default_home();
        let home = env::var_os("SSU_HOME")
            .filter(|v| !v.is_empty())
            .map(|custom| {
                std::path::absolute(PathBuf::from(&custom))
                    .unwrap_or_else(|_| PathBuf::from(&custom))
            })
            .or_else(home_beside_executable)
            .unwrap_or_else(|| standard.clone());
        let tag = (key_of(&home) != key_of(&standard))
            .then(|| format!("{:08x}", win::fnv1a(&key_of(&home)) as u32));
        Instance { home, tag }
    }

    pub fn state_path(&self) -> PathBuf {
        self.home.join("state.ini")
    }

    pub fn log_path(&self) -> PathBuf {
        self.home.join("updater.log")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.home.join("backup")
    }

    /// Where copies of the files found in one particular game folder are kept.
    ///
    /// Backups are kept per game folder: a file saved from one installation
    /// must never be mistaken for, or restored into, another.
    pub fn backup_dir_for(&self, game_root: &std::path::Path) -> PathBuf {
        self.backup_dir()
            .join(format!("{:08x}", win::fnv1a(&key_of(game_root)) as u32))
    }

    pub fn installed_exe(&self) -> PathBuf {
        self.home.join(EXE_NAME)
    }

    pub fn display_name(&self) -> String {
        match &self.tag {
            None => APP_NAME.to_string(),
            Some(tag) => format!("{APP_NAME} (dev {tag})"),
        }
    }

    /// The name of the scheduled task.
    ///
    /// Task names are shared by everyone on the PC, so the standard
    /// installation carries the account's id: a second person installing the
    /// updater gets a task of their own instead of colliding with the first.
    pub fn task_name(&self) -> String {
        match (&self.tag, win::current_user_sid()) {
            (None, Ok(sid)) => format!("{APP_NAME}-{sid}"),
            _ => self.display_name(),
        }
    }

    pub fn registry_key(&self) -> String {
        match &self.tag {
            None => APP_ID.to_string(),
            Some(tag) => format!("{APP_ID}-dev-{tag}"),
        }
    }

    pub fn lock_name(&self) -> String {
        format!("Local\\{APP_ID}-{:016x}", win::fnv1a(&key_of(&self.home)))
    }
}

fn key_of(path: &std::path::Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// An installed copy keeps its state file next to the executable. Recognising
/// that is what binds the scheduled task to its own folder: a task cannot see
/// environment overrides, but it always runs the copy that was installed.
fn home_beside_executable() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    let state = std::fs::read(dir.join("state.ini")).ok()?;
    crate::state::is_state_file(&state).then(|| dir.to_path_buf())
}

fn default_home() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .filter(|v| !v.is_empty())
                .map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(home: &str, tag: Option<&str>) -> Instance {
        Instance {
            home: PathBuf::from(home),
            tag: tag.map(str::to_string),
        }
    }

    #[test]
    fn the_standard_task_name_is_unique_to_the_account() {
        let sid = win::current_user_sid().unwrap();
        assert_eq!(
            instance(r"C:\Users\me\AppData\Local\StarStringsUpdater", None).task_name(),
            format!("StarStrings Updater-{sid}")
        );
        // A development copy is already unique through its folder.
        assert_eq!(
            instance(r"C:\dev\home", Some("1a2b3c4d")).task_name(),
            "StarStrings Updater (dev 1a2b3c4d)"
        );
    }

    #[test]
    fn backups_are_kept_per_game_folder() {
        let inst = instance(r"C:\data", None);
        let here = inst.backup_dir_for(std::path::Path::new(r"D:\Games\StarCitizen"));
        let same = inst.backup_dir_for(std::path::Path::new(r"d:\games\STARCITIZEN"));
        let elsewhere = inst.backup_dir_for(std::path::Path::new(r"E:\Games\StarCitizen"));
        assert_eq!(here, same);
        assert_ne!(here, elsewhere);
        assert!(here.starts_with(inst.backup_dir()));
        assert_eq!(here.parent(), Some(inst.backup_dir().as_path()));
    }

    #[test]
    fn downloads_are_given_two_minutes_by_default() {
        assert_eq!(download_timeout(), std::time::Duration::from_secs(120));
    }

    #[test]
    fn channels_resolve_case_insensitively() {
        assert_eq!(channel("live").map(|c| c.name), Some("LIVE"));
        assert_eq!(channel("PTU").map(|c| c.name), Some("PTU"));
        assert!(channel("EPTU").is_none());
    }

    #[test]
    fn default_sources_are_https_github_release_assets() {
        for c in &CHANNELS {
            assert!(
                c.url
                    .starts_with("https://github.com/MrKraken/StarStrings/releases/download/")
            );
            assert!(c.url.ends_with(".zip"));
        }
    }

    #[test]
    fn user_agent_carries_only_name_and_version() {
        assert_eq!(USER_AGENT, format!("starstrings-updater/{VERSION}"));
    }
}
