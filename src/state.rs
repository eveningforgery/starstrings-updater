//! What the updater remembers between runs.
//!
//! Stored as plain `key=value` lines so it can be read (and, if ever needed,
//! fixed) with a text editor. Parsing never fails: unknown or damaged lines are
//! ignored, which at worst costs one extra download.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// First line of every state file; also how an installed copy recognises its own folder.
const HEADER: &str = "# StarStrings Updater state";

/// Whether `bytes` is a state file written by this program.
pub fn is_state_file(bytes: &[u8]) -> bool {
    bytes.starts_with(HEADER.as_bytes())
}

/// A file this updater put into the game folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFile {
    /// Path relative to the channel folder, e.g. `Data\Localization\english\global.ini`.
    pub rel: String,
    pub size: u64,
    pub crc32: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChannelState {
    /// Validators of the release archive that is currently installed.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unix time of the last full download.
    pub fetched_at: Option<u64>,
    /// Local time at which files were last written.
    pub updated_at: Option<String>,
    pub files: Vec<InstalledFile>,
    /// The `g_language` line in USER.cfg was added by the updater.
    pub cfg_added: bool,
    /// USER.cfg did not exist and was created by the updater.
    pub cfg_created: bool,
    /// The `g_language` value that was there before the updater changed it.
    pub cfg_previous: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    /// The folder containing the channel folders (`...\StarCitizen`).
    pub game_root: Option<PathBuf>,
    /// Channels to keep up to date, e.g. `["LIVE"]`.
    pub channels: Vec<String>,
    pub per_channel: BTreeMap<String, ChannelState>,
}

/// Makes a value safe to store on one line.
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect::<String>()
        .trim()
        .to_string()
}

fn valid_channel_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl State {
    pub fn parse(text: &str) -> State {
        let mut state = State::default();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key.split_once('.') {
                None => match key {
                    "game_root" if !value.is_empty() => {
                        state.game_root = Some(PathBuf::from(value))
                    }
                    "channels" => {
                        state.channels = value
                            .split(',')
                            .map(|c| c.trim().to_ascii_uppercase())
                            .filter(|c| valid_channel_name(c))
                            .fold(Vec::new(), |mut acc, c| {
                                if !acc.contains(&c) {
                                    acc.push(c);
                                }
                                acc
                            });
                    }
                    _ => {}
                },
                Some((channel, field)) if valid_channel_name(channel) => {
                    let cs = state.per_channel.entry(channel.to_string()).or_default();
                    let text = || Some(value.to_string()).filter(|v| !v.is_empty());
                    match field {
                        "etag" => cs.etag = text(),
                        "last_modified" => cs.last_modified = text(),
                        "fetched_at" => cs.fetched_at = value.parse().ok(),
                        "updated_at" => cs.updated_at = text(),
                        "cfg_added" => cs.cfg_added = value == "1",
                        "cfg_created" => cs.cfg_created = value == "1",
                        "cfg_previous" => cs.cfg_previous = text(),
                        "file" => {
                            let mut parts = value.splitn(3, '|');
                            if let (Some(size), Some(crc), Some(rel)) =
                                (parts.next(), parts.next(), parts.next())
                                && let (Ok(size), Ok(crc32)) =
                                    (size.parse::<u64>(), u32::from_str_radix(crc, 16))
                                && !rel.is_empty()
                            {
                                cs.files.push(InstalledFile {
                                    rel: rel.to_string(),
                                    size,
                                    crc32,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                Some(_) => {}
            }
        }
        state
    }

    pub fn serialize(&self) -> String {
        let mut out = format!("{HEADER}. Managed automatically.\n");
        if let Some(root) = &self.game_root {
            out.push_str(&format!("game_root={}\n", clean(&root.to_string_lossy())));
        }
        out.push_str(&format!("channels={}\n", self.channels.join(",")));
        for (name, cs) in &self.per_channel {
            let mut put = |field: &str, value: &str| {
                out.push_str(&format!("{name}.{field}={}\n", clean(value)));
            };
            if let Some(v) = &cs.etag {
                put("etag", v);
            }
            if let Some(v) = &cs.last_modified {
                put("last_modified", v);
            }
            if let Some(v) = cs.fetched_at {
                put("fetched_at", &v.to_string());
            }
            if let Some(v) = &cs.updated_at {
                put("updated_at", v);
            }
            for f in &cs.files {
                put("file", &format!("{}|{:08x}|{}", f.size, f.crc32, f.rel));
            }
            if cs.cfg_added {
                put("cfg_added", "1");
            }
            if cs.cfg_created {
                put("cfg_created", "1");
            }
            if let Some(v) = &cs.cfg_previous {
                put("cfg_previous", v);
            }
        }
        out
    }

    /// `Ok(None)` when no state has been saved yet.
    pub fn load(path: &Path) -> io::Result<Option<State>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(State::parse(&String::from_utf8_lossy(&bytes)))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes the state atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("ini.tmp");
        fs::write(&tmp, self.serialize())?;
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// The per-channel record, created on first use.
    pub fn channel_mut(&mut self, name: &str) -> &mut ChannelState {
        self.per_channel.entry(name.to_string()).or_default()
    }

    /// Enables `name` if it is not already enabled.
    pub fn enable(&mut self, name: &str) {
        if !self.channels.iter().any(|c| c == name) {
            self.channels.push(name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        let mut state = State {
            game_root: Some(PathBuf::from(
                r"C:\Program Files\Roberts Space Industries\StarCitizen",
            )),
            channels: vec!["LIVE".into(), "PTU".into()],
            ..Default::default()
        };
        let live = state.channel_mut("LIVE");
        live.etag = Some("\"0x8DF40E2D5861898\"".into());
        live.last_modified = Some("Thu, 12 Nov 2026 17:31:49 GMT".into());
        live.fetched_at = Some(1_794_500_000);
        live.updated_at = Some("2026-11-12 22:41:03".into());
        live.files.push(InstalledFile {
            rel: r"Data\Localization\english\global.ini".into(),
            size: 11_034_351,
            crc32: 0x0108_fb46,
        });
        live.cfg_added = true;
        let ptu = state.channel_mut("PTU");
        ptu.cfg_previous = Some("french_(france)".into());
        ptu.cfg_created = true;
        state
    }

    #[test]
    fn round_trips() {
        let state = sample();
        assert_eq!(State::parse(&state.serialize()), state);
    }

    #[test]
    fn empty_and_default_round_trip() {
        assert_eq!(State::parse(""), State::default());
        assert_eq!(
            State::parse(&State::default().serialize()),
            State::default()
        );
    }

    #[test]
    fn tolerates_garbage() {
        let text = "\u{feff}nonsense\n=novalue\n#comment\nLIVE.file=notanumber|zz|x\nLIVE.file=5|00ff\n\
                    lower.etag=x\nLIVE.unknown=1\ngame_root=\nchannels=live, ptu ,,bad name,LIVE\r\n\
                    LIVE.etag=abc\r\n";
        let state = State::parse(text);
        assert_eq!(state.game_root, None);
        assert_eq!(state.channels, vec!["LIVE".to_string(), "PTU".to_string()]);
        assert_eq!(state.per_channel["LIVE"].etag.as_deref(), Some("abc"));
        assert!(state.per_channel["LIVE"].files.is_empty());
        assert!(!state.per_channel.contains_key("lower"));
    }

    #[test]
    fn values_cannot_inject_lines() {
        let mut state = State::default();
        state.channel_mut("LIVE").etag =
            Some("abc\r\nLIVE.cfg_created=1\ngame_root=C:\\evil".into());
        let parsed = State::parse(&state.serialize());
        assert_eq!(parsed.game_root, None);
        assert!(!parsed.per_channel["LIVE"].cfg_created);
        assert_eq!(
            parsed.per_channel["LIVE"].etag.as_deref(),
            Some("abcLIVE.cfg_created=1game_root=C:\\evil")
        );
    }

    #[test]
    fn file_paths_may_contain_separators_used_by_the_format() {
        let mut state = State::default();
        state.channel_mut("LIVE").files.push(InstalledFile {
            rel: r"Data\Localization\a|b=c\global.ini".into(),
            size: 1,
            crc32: 2,
        });
        assert_eq!(State::parse(&state.serialize()), state);
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("ssu-state-{}", std::process::id()));
        let path = dir.join("nested").join("state.ini");
        assert_eq!(State::load(&path).unwrap(), None);
        let state = sample();
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), Some(state));
        assert!(!path.with_extension("ini.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recognises_its_own_files() {
        assert!(is_state_file(sample().serialize().as_bytes()));
        assert!(is_state_file(State::default().serialize().as_bytes()));
        assert!(!is_state_file(b""));
        assert!(!is_state_file(b"[settings]\nkey=value\n"));
    }

    #[test]
    fn enable_is_idempotent() {
        let mut state = State::default();
        state.enable("LIVE");
        state.enable("LIVE");
        state.enable("PTU");
        assert_eq!(state.channels, vec!["LIVE".to_string(), "PTU".to_string()]);
    }
}
