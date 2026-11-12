//! Finding the Star Citizen installation.
//!
//! The RSI Launcher lets players put the game anywhere, and it keeps its own
//! settings in an encrypted store. Its plain-text log, however, mentions the
//! install folder every time the game is installed, updated or launched, so
//! that is the primary source; well-known default locations are the fallback.

use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config;
use crate::win;

/// Folder names the launcher uses for the game's release channels.
const CHANNEL_DIRS: [&str; 5] = ["LIVE", "PTU", "EPTU", "HOTFIX", "TECH-PREVIEW"];
/// The launcher always installs the game into a folder of this name.
const INSTALL_DIR: &str = "StarCitizen";
/// Launcher data folders under %APPDATA% (release and release-candidate launcher).
const LAUNCHER_DIRS: [&str; 2] = ["rsilauncher", "rsirclauncher"];
/// Only the tail of a launcher log is read; the newest entries are what matter.
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// A channel folder that actually contains (part of) a game installation.
pub fn is_channel_dir(path: &Path) -> bool {
    path.is_dir()
        && (path.join("Data.p4k").is_file()
            || path.join("Bin64").is_dir()
            || path.join("build_manifest.id").is_file())
}

/// A folder holding at least one installed channel.
pub fn is_game_root(path: &Path) -> bool {
    CHANNEL_DIRS
        .iter()
        .any(|channel| is_channel_dir(&path.join(channel)))
}

/// Turns whatever folder a person points at into the game root.
///
/// Accepts the root itself (`...\StarCitizen`), a channel folder inside it
/// (`...\StarCitizen\LIVE`) or the library folder above it.
pub fn normalize_root(path: &Path) -> Option<PathBuf> {
    if is_game_root(path) {
        return Some(path.to_path_buf());
    }
    if is_channel_dir(path)
        && let Some(parent) = path.parent()
        && is_game_root(parent)
    {
        return Some(parent.to_path_buf());
    }
    let inside = path.join(INSTALL_DIR);
    is_game_root(&inside).then_some(inside)
}

/// Extracts candidate game roots from launcher log text, oldest first.
///
/// The log is JSON-ish with escaped backslashes. Every absolute Windows path
/// that has a `StarCitizen` component is cut off after that component; whether
/// the result is real is decided later against the file system.
pub fn roots_in_log(text: &str) -> Vec<String> {
    let text = text.replace("\\\\", "\\").replace('/', "\\");
    let bytes = text.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut at = 0;
    while at + 2 < bytes.len() {
        let starts_path = bytes[at].is_ascii_alphabetic()
            && bytes[at + 1] == b':'
            && bytes[at + 2] == b'\\'
            && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric());
        if !starts_path {
            at += 1;
            continue;
        }
        let end = bytes[at..]
            .iter()
            .position(|b| {
                matches!(
                    b,
                    b'"' | b'\''
                        | b'\n'
                        | b'\r'
                        | b'('
                        | b')'
                        | b','
                        | b';'
                        | b'|'
                        | b'<'
                        | b'>'
                        | b'*'
                        | b'?'
                )
            })
            .map_or(bytes.len(), |i| at + i);
        let path = text[at..end].trim();
        let parts: Vec<&str> = path.split('\\').collect();
        // Cut after the first and after the last `StarCitizen` component: a
        // library folder may itself be called StarCitizen.
        let positions: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.trim().eq_ignore_ascii_case(INSTALL_DIR))
            .map(|(index, _)| index)
            .collect();
        for index in [positions.first(), positions.last()].into_iter().flatten() {
            let root = parts[..=*index].join("\\");
            // Keep the list ordered by most recent mention.
            found.retain(|known| !known.eq_ignore_ascii_case(&root));
            found.push(root);
        }
        at = end.max(at + 1);
    }
    found
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length > LOG_TAIL_BYTES {
        file.seek(SeekFrom::Start(length - LOG_TAIL_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn launcher_logs() -> Vec<PathBuf> {
    let Some(appdata) = env::var_os("APPDATA").filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let mut logs: Vec<(SystemTime, PathBuf)> = LAUNCHER_DIRS
        .iter()
        .map(|dir| Path::new(&appdata).join(dir).join("logs").join("log.log"))
        .filter_map(|path| {
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    // Most recently used launcher first.
    logs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    logs.into_iter().map(|(_, path)| path).collect()
}

fn default_locations() -> Vec<PathBuf> {
    const RELATIVE: [&str; 6] = [
        r"Program Files\Roberts Space Industries\StarCitizen",
        r"Roberts Space Industries\StarCitizen",
        r"Games\Roberts Space Industries\StarCitizen",
        r"Games\StarCitizen",
        r"StarCitizen",
        r"Program Files (x86)\Roberts Space Industries\StarCitizen",
    ];
    win::fixed_drives()
        .iter()
        .flat_map(|drive| RELATIVE.iter().map(move |rel| Path::new(drive).join(rel)))
        .collect()
}

/// Picks the best root among `candidates` (given in order of preference):
/// the first with LIVE installed, otherwise the first valid one.
fn choose(candidates: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    let mut fallback = None;
    for candidate in candidates {
        let Some(root) = normalize_root(&candidate) else {
            continue;
        };
        if is_channel_dir(&root.join(config::LIVE)) {
            return Some(root);
        }
        fallback.get_or_insert(root);
    }
    fallback
}

/// Locates the game root, or `None` if Star Citizen cannot be found.
///
/// `SSU_SEARCH_PATHS` (a `;`-separated list of folders) replaces the automatic
/// search entirely. The test-suite uses it so that it can never find, let alone
/// touch, a real installation.
pub fn detect() -> Option<PathBuf> {
    if let Some(list) = env::var_os("SSU_SEARCH_PATHS") {
        return choose(env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    let from_logs = launcher_logs().into_iter().flat_map(|log| {
        let roots = read_tail(&log)
            .map(|text| roots_in_log(&text))
            .unwrap_or_default();
        // Newest mention first.
        roots.into_iter().rev().map(PathBuf::from)
    });
    choose(from_logs.chain(default_locations()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            let path = env::temp_dir().join(format!(
                "ssu-detect-{label}-{}-{}",
                std::process::id(),
                win::unix_now()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_channel(root: &Path, channel: &str) {
        let dir = root.join(channel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Data.p4k"), b"").unwrap();
    }

    #[test]
    fn recognises_channel_folders_and_roots() {
        let tmp = TempDir::new("recognise");
        let root = tmp.0.join("Library").join("StarCitizen");
        fs::create_dir_all(root.join("EMPTY")).unwrap();
        assert!(!is_game_root(&root));
        make_channel(&root, "LIVE");
        assert!(is_channel_dir(&root.join("LIVE")));
        assert!(!is_channel_dir(&root.join("EMPTY")));
        assert!(!is_channel_dir(&root.join("MISSING")));
        assert!(is_game_root(&root));

        // A channel is also recognised by its binaries or manifest alone.
        fs::create_dir_all(root.join("PTU").join("Bin64")).unwrap();
        assert!(is_channel_dir(&root.join("PTU")));
        fs::create_dir_all(root.join("EPTU")).unwrap();
        fs::write(root.join("EPTU").join("build_manifest.id"), b"{}").unwrap();
        assert!(is_channel_dir(&root.join("EPTU")));
    }

    #[test]
    fn normalizes_whatever_folder_was_picked() {
        let tmp = TempDir::new("normalize");
        let library = tmp.0.join("Roberts Space Industries");
        let root = library.join("StarCitizen");
        make_channel(&root, "LIVE");

        assert_eq!(normalize_root(&root), Some(root.clone()));
        assert_eq!(normalize_root(&root.join("LIVE")), Some(root.clone()));
        assert_eq!(normalize_root(&library), Some(root.clone()));
        assert_eq!(normalize_root(&tmp.0), None);
        assert_eq!(normalize_root(&root.join("LIVE").join("Bin64")), None);
        assert_eq!(normalize_root(&tmp.0.join("does-not-exist")), None);
    }

    #[test]
    fn prefers_a_root_that_has_live() {
        let tmp = TempDir::new("choose");
        let ptu_only = tmp.0.join("a").join("StarCitizen");
        let with_live = tmp.0.join("b").join("StarCitizen");
        make_channel(&ptu_only, "PTU");
        make_channel(&with_live, "LIVE");

        let missing = tmp.0.join("nope");
        assert_eq!(
            choose([missing.clone(), ptu_only.clone(), with_live.clone()].into_iter()),
            Some(with_live.clone())
        );
        assert_eq!(
            choose([missing.clone(), ptu_only.clone()].into_iter()),
            Some(ptu_only)
        );
        assert_eq!(choose([missing].into_iter()), None);
    }

    #[test]
    fn finds_roots_in_real_launcher_log_lines() {
        let log = r#"
{ "t":"2026-04-14 21:03:22.739", "[main][info] ": "[Pipeline] Installing Star Citizen LIVE 4.7.1-live.11617053 at C:\\Program Files\\Roberts Space Industries\\StarCitizen (type: update, forceDP: false)"  },
{ "t":"2026-04-14 21:03:22.741", "[main][info] ": "[LauncherSupport::executeCommand] Sending command: fix-permission --path \"C:\\Program Files\\Roberts Space Industries\\StarCitizen\" (as sudo : true)"  },
{ "t":"2026-04-14 21:22:57.633", "[main][info] ": "[Launcher::launch] Launching Star Citizen LIVE from (C:\\Program Files\\Roberts Space Industries\\StarCitizen\\LIVE)"  },
{ "t":"2026-04-14 21:23:32.197", "[main][info] ": "[API] request: https://robertsspaceindustries.com/api/launcher/v3/games/library"  },
"#;
        assert_eq!(
            roots_in_log(log),
            vec![r"C:\Program Files\Roberts Space Industries\StarCitizen".to_string()]
        );
    }

    #[test]
    fn handles_odd_paths_in_logs() {
        // Mixed separators, as the launcher itself writes in error messages.
        assert_eq!(
            roots_in_log(r#""IO ERROR [D:\\Games\\StarCitizen\\EPTU/Bin64\\bink2w64.dll]: Error""#),
            vec![r"D:\Games\StarCitizen".to_string()]
        );
        // A library folder that is itself called StarCitizen yields both cuts.
        assert_eq!(
            roots_in_log(r#"from (E:\\StarCitizen\\StarCitizen\\LIVE)"#),
            vec![
                r"E:\StarCitizen".to_string(),
                r"E:\StarCitizen\StarCitizen".to_string()
            ]
        );
        // The executable name must not be mistaken for the folder.
        assert_eq!(
            roots_in_log(r#""C:\\SC\\StarCitizen\\LIVE\\Bin64\\StarCitizen.exe""#),
            vec![r"C:\SC\StarCitizen".to_string()]
        );
        // Later mentions move a root to the end (most recent last).
        assert_eq!(
            roots_in_log(
                "in (C:\\\\A\\\\StarCitizen\\\\LIVE)\nin (D:\\\\B\\\\StarCitizen\\\\LIVE)\nin (C:\\\\A\\\\StarCitizen\\\\PTU)"
            ),
            vec![
                r"D:\B\StarCitizen".to_string(),
                r"C:\A\StarCitizen".to_string()
            ]
        );
        // Nothing relevant.
        assert!(
            roots_in_log("https://robertsspaceindustries.com/api C:\\\\Windows\\\\System32")
                .is_empty()
        );
        assert!(roots_in_log("").is_empty());
        assert!(roots_in_log("C:").is_empty());
    }

    #[test]
    fn reads_only_the_tail_of_large_logs() {
        let tmp = TempDir::new("tail");
        let path = tmp.0.join("log.log");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(read_tail(&path).as_deref(), Some("hello"));
        assert_eq!(read_tail(&tmp.0.join("missing.log")), None);
    }

    #[test]
    fn default_locations_cover_the_standard_install_path() {
        let all = default_locations();
        assert!(all.iter().any(|p| {
            p.to_string_lossy()
                .ends_with(r"Program Files\Roberts Space Industries\StarCitizen")
        }));
    }
}
