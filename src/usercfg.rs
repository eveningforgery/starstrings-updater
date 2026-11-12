//! Editing the game's `USER.cfg` without disturbing anything else in it.
//!
//! StarStrings only takes effect when the game is told to use the English
//! localization folder, which is one line: `g_language = english`. Players
//! often keep their own settings in this file, so it is never replaced - the
//! one line is added, and every other byte is preserved exactly.

const KEY: &str = "g_language";
pub const LANGUAGE: &str = "english";
const MAX_SIZE: usize = 1024 * 1024;

/// What `ensure` did to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    /// The line was missing and has been appended.
    Added,
    /// The line named another language and has been pointed at English.
    Replaced {
        previous: String,
    },
}

struct Line<'a> {
    body: &'a [u8],
    eol: &'a [u8],
}

fn split_lines(content: &[u8]) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let end = content[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(content.len(), |i| start + i + 1);
        let raw = &content[start..end];
        let (body, eol) = if raw.ends_with(b"\r\n") {
            raw.split_at(raw.len() - 2)
        } else if raw.ends_with(b"\n") {
            raw.split_at(raw.len() - 1)
        } else {
            (raw, &raw[raw.len()..])
        };
        lines.push(Line { body, eol });
        start = end;
    }
    lines
}

/// The language a line assigns to `g_language`, if it is such a line.
fn assigned_language(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.starts_with(';')
        || text.starts_with("--")
        || text.starts_with("//")
        || text.starts_with('#')
    {
        return None;
    }
    // Both `name = value` and the console form `name value` occur in the wild.
    let (key, value) = match text.split_once('=') {
        Some(pair) => pair,
        None => text.split_once(char::is_whitespace)?,
    };
    if !key.trim().eq_ignore_ascii_case(KEY) {
        return None;
    }
    Some(value.trim().trim_matches('"').trim().to_string())
}

fn check_editable(content: &[u8]) -> Result<(), String> {
    if content.len() > MAX_SIZE {
        return Err("USER.cfg is unexpectedly large; leaving it untouched".to_string());
    }
    if content.starts_with(&[0xFF, 0xFE])
        || content.starts_with(&[0xFE, 0xFF])
        || content.contains(&0)
    {
        return Err(
            "USER.cfg is not a plain text file (UTF-16?); leaving it untouched".to_string(),
        );
    }
    Ok(())
}

fn our_line() -> Vec<u8> {
    format!("{KEY} = {LANGUAGE}").into_bytes()
}

/// The line ending the file already uses (Windows style if it has none).
fn eol_of(content: &[u8]) -> &'static [u8] {
    match content.iter().position(|b| *b == b'\n') {
        Some(i) if i == 0 || content[i - 1] != b'\r' => b"\n",
        _ => b"\r\n",
    }
}

fn rebuild(lines: &[Line<'_>], replace: usize, with: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index == replace {
            if let Some(body) = with {
                out.extend_from_slice(body);
                out.extend_from_slice(line.eol);
            }
        } else {
            out.extend_from_slice(line.body);
            out.extend_from_slice(line.eol);
        }
    }
    out
}

fn last_assignment(lines: &[Line<'_>]) -> Option<(usize, String)> {
    lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, line)| assigned_language(line.body).map(|value| (index, value)))
}

/// Makes sure the file selects the English localization.
///
/// A missing line is always added. A line naming a different language is the
/// player's own choice and is only changed when `force` is set, which the
/// installer does once because installing StarStrings is asking for exactly that.
pub fn ensure(content: &[u8], force: bool) -> Result<(Vec<u8>, Change), String> {
    check_editable(content)?;
    let lines = split_lines(content);
    match last_assignment(&lines) {
        Some((_, value)) if value.eq_ignore_ascii_case(LANGUAGE) || !force => {
            Ok((content.to_vec(), Change::Unchanged))
        }
        Some((index, previous)) => Ok((
            rebuild(&lines, index, Some(&our_line())),
            Change::Replaced { previous },
        )),
        None => {
            let mut out = content.to_vec();
            let ends_with_newline = content.ends_with(b"\n");
            if !content.is_empty() && !ends_with_newline {
                out.extend_from_slice(eol_of(content));
            }
            out.extend_from_slice(&our_line());
            if ends_with_newline {
                out.extend_from_slice(eol_of(content));
            }
            Ok((out, Change::Added))
        }
    }
}

/// Undoes `ensure`: restores `previous` if there was one, otherwise removes the
/// line. Does nothing if the file no longer selects English.
pub fn revert(content: &[u8], previous: Option<&str>) -> Result<Vec<u8>, String> {
    check_editable(content)?;
    let lines = split_lines(content);
    match last_assignment(&lines) {
        Some((index, value)) if value.eq_ignore_ascii_case(LANGUAGE) => {
            let restored = previous.map(|p| format!("{KEY} = {p}").into_bytes());
            Ok(rebuild(&lines, index, restored.as_deref()))
        }
        _ => Ok(content.to_vec()),
    }
}

/// The language the file currently selects, if any.
pub fn current_language(content: &[u8]) -> Option<String> {
    last_assignment(&split_lines(content)).map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_str(content: &str, force: bool) -> (String, Change) {
        let (bytes, change) = ensure(content.as_bytes(), force).unwrap();
        (String::from_utf8(bytes).unwrap(), change)
    }

    #[test]
    fn creates_the_file_exactly_like_upstream_ships_it() {
        assert_eq!(
            ensure_str("", false),
            ("g_language = english".into(), Change::Added)
        );
    }

    #[test]
    fn leaves_a_correct_file_alone() {
        for content in [
            "g_language = english",
            "g_language=english\r\n",
            "  G_Language   =   English  \n",
            "g_language = \"english\"",
            "g_language english",
            "r_DisplayInfo = 1\r\ng_language = english\r\nsys_maxFPS = 120\r\n",
            "\u{feff}g_language = english\n",
        ] {
            assert_eq!(
                ensure_str(content, false),
                (content.into(), Change::Unchanged),
                "{content:?}"
            );
            assert_eq!(
                ensure_str(content, true),
                (content.into(), Change::Unchanged),
                "{content:?}"
            );
        }
    }

    #[test]
    fn appends_while_preserving_every_existing_byte() {
        // No trailing newline: add one separator, keep the file newline-less.
        assert_eq!(
            ensure_str("r_DisplayInfo = 1", false),
            (
                "r_DisplayInfo = 1\r\ng_language = english".into(),
                Change::Added
            )
        );
        // Windows line endings.
        assert_eq!(
            ensure_str("r_DisplayInfo = 1\r\nsys_maxFPS = 120\r\n", false),
            (
                "r_DisplayInfo = 1\r\nsys_maxFPS = 120\r\ng_language = english\r\n".into(),
                Change::Added
            )
        );
        // Unix line endings.
        assert_eq!(
            ensure_str("a = 1\nb = 2\n", false),
            ("a = 1\nb = 2\ng_language = english\n".into(), Change::Added)
        );
        assert_eq!(
            ensure_str("a = 1\nb = 2", false),
            ("a = 1\nb = 2\ng_language = english".into(), Change::Added)
        );
    }

    #[test]
    fn commented_out_lines_do_not_count() {
        for comment in [
            ";g_language = german",
            "-- g_language = german",
            "// g_language = german",
            "# g_language = german",
        ] {
            let (out, change) = ensure_str(comment, false);
            assert_eq!(change, Change::Added, "{comment}");
            assert_eq!(out, format!("{comment}\r\ng_language = english"));
        }
    }

    #[test]
    fn similar_keys_are_not_confused_with_ours() {
        let (out, change) = ensure_str("g_languageAudio = french\ng_language_x = 1\n", false);
        assert_eq!(change, Change::Added);
        assert_eq!(
            out,
            "g_languageAudio = french\ng_language_x = 1\ng_language = english\n"
        );
    }

    #[test]
    fn another_language_is_respected_unless_forced() {
        let content = "sys_maxFPS = 120\r\ng_language = french_(france)\r\nr_x = 1\r\n";
        assert_eq!(
            ensure_str(content, false),
            (content.into(), Change::Unchanged)
        );
        assert_eq!(
            ensure_str(content, true),
            (
                "sys_maxFPS = 120\r\ng_language = english\r\nr_x = 1\r\n".into(),
                Change::Replaced {
                    previous: "french_(france)".into()
                }
            )
        );
    }

    #[test]
    fn only_the_effective_assignment_is_replaced() {
        let content = "g_language = german\ng_language = spanish\n";
        assert_eq!(
            ensure_str(content, true),
            (
                "g_language = german\ng_language = english\n".into(),
                Change::Replaced {
                    previous: "spanish".into()
                }
            )
        );
        // English already wins, so the earlier line does not matter.
        let content = "g_language = german\ng_language = english\n";
        assert_eq!(
            ensure_str(content, true),
            (content.into(), Change::Unchanged)
        );
    }

    #[test]
    fn non_utf8_bytes_survive_untouched() {
        let mut content = b"; caf\xe9 settings\r\nr_x = 1\r\n".to_vec();
        let (out, change) = ensure(&content, false).unwrap();
        assert_eq!(change, Change::Added);
        content.extend_from_slice(b"g_language = english\r\n");
        assert_eq!(out, content);
    }

    #[test]
    fn refuses_files_it_cannot_edit_safely() {
        assert!(ensure(&[0xFF, 0xFE, b'g', 0, b'_', 0], false).is_err());
        assert!(ensure(&[0xFE, 0xFF, 0, b'g'], false).is_err());
        assert!(ensure(b"g_language\0 = english", false).is_err());
        assert!(ensure(&vec![b'a'; MAX_SIZE + 1], false).is_err());
    }

    #[test]
    fn revert_removes_an_added_line() {
        for original in ["", "r_x = 1\r\n", "a = 1\nb = 2\n", "r_x = 1"] {
            let (with_line, change) = ensure(original.as_bytes(), false).unwrap();
            assert_eq!(change, Change::Added);
            let reverted = revert(&with_line, None).unwrap();
            // The only tolerated difference is the separator we had to add.
            let reverted = String::from_utf8(reverted).unwrap();
            assert_eq!(reverted.trim_end(), original.trim_end(), "{original:?}");
        }
    }

    #[test]
    fn revert_restores_a_replaced_language() {
        let original = "sys_maxFPS = 120\r\ng_language = french_(france)\r\n";
        let (changed, change) = ensure(original.as_bytes(), true).unwrap();
        let Change::Replaced { previous } = change else {
            panic!("expected a replacement")
        };
        assert_eq!(
            revert(&changed, Some(&previous)).unwrap(),
            original.as_bytes()
        );
    }

    #[test]
    fn revert_leaves_a_file_the_player_changed_since() {
        let content = b"g_language = german\n";
        assert_eq!(revert(content, None).unwrap(), content);
        assert_eq!(revert(content, Some("french")).unwrap(), content);
    }

    #[test]
    fn reports_the_current_language() {
        assert_eq!(current_language(b""), None);
        assert_eq!(current_language(b"r_x = 1"), None);
        assert_eq!(
            current_language(b"g_language = English").as_deref(),
            Some("English")
        );
        assert_eq!(
            current_language(b"g_language = a\ng_language = b").as_deref(),
            Some("b")
        );
    }
}
