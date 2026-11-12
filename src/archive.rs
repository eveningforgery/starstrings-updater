//! A deliberately small, read-only ZIP reader, plus the policy that decides
//! which archive entries may be installed.
//!
//! The archive arrives from the network and is processed unattended, so it is
//! treated as hostile: every offset is bounds-checked, sizes are capped, each
//! extracted file must match its recorded size and CRC, and entry names go
//! through a strict allow-list rather than a deny-list.

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const EOCD_LEN: usize = 22;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;
const MAX_ENTRIES: usize = 4096;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const FLAG_ENCRYPTED: u16 = 0x0001;

/// One file or folder recorded in the archive's central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub size: u64,
    pub header_offset: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Lists the entries of a ZIP archive held in memory.
pub fn entries(zip: &[u8]) -> Result<Vec<Entry>, String> {
    let not_zip = || "the download is not a ZIP archive".to_string();
    let corrupt = || "the archive is truncated or corrupt".to_string();
    let zip64 = || "ZIP64 archives are not supported".to_string();

    if zip.len() < EOCD_LEN {
        return Err(not_zip());
    }
    // The end-of-central-directory record sits at the very end, possibly
    // followed by a comment of up to 65535 bytes.
    let lowest = zip.len().saturating_sub(EOCD_LEN + u16::MAX as usize);
    let mut at = zip.len() - EOCD_LEN;
    let eocd = loop {
        if u32_at(zip, at) == Some(EOCD_SIGNATURE) {
            let comment = u16_at(zip, at + 20).ok_or_else(corrupt)? as usize;
            if at + EOCD_LEN + comment <= zip.len() {
                break at;
            }
        }
        if at == lowest {
            return Err(not_zip());
        }
        at -= 1;
    };

    let disk = u16_at(zip, eocd + 4).ok_or_else(corrupt)?;
    let directory_disk = u16_at(zip, eocd + 6).ok_or_else(corrupt)?;
    let entries_here = u16_at(zip, eocd + 8).ok_or_else(corrupt)?;
    let total = u16_at(zip, eocd + 10).ok_or_else(corrupt)?;
    let directory_size = u32_at(zip, eocd + 12).ok_or_else(corrupt)?;
    let directory_offset = u32_at(zip, eocd + 16).ok_or_else(corrupt)?;

    if total == u16::MAX || directory_size == u32::MAX || directory_offset == u32::MAX {
        return Err(zip64());
    }
    if disk != 0 || directory_disk != 0 || entries_here != total {
        return Err("multi-part ZIP archives are not supported".to_string());
    }
    if total as usize > MAX_ENTRIES {
        return Err("the archive has too many entries".to_string());
    }
    let start = directory_offset as usize;
    let end = start
        .checked_add(directory_size as usize)
        .ok_or_else(corrupt)?;
    if end > eocd {
        return Err(corrupt());
    }

    let mut out = Vec::with_capacity(total as usize);
    let mut at = start;
    for _ in 0..total {
        if at + CENTRAL_LEN > end || u32_at(zip, at) != Some(CENTRAL_SIGNATURE) {
            return Err(corrupt());
        }
        let flags = u16_at(zip, at + 8).ok_or_else(corrupt)?;
        let method = u16_at(zip, at + 10).ok_or_else(corrupt)?;
        let crc32 = u32_at(zip, at + 16).ok_or_else(corrupt)?;
        let compressed_size = u32_at(zip, at + 20).ok_or_else(corrupt)?;
        let size = u32_at(zip, at + 24).ok_or_else(corrupt)?;
        let name_len = u16_at(zip, at + 28).ok_or_else(corrupt)? as usize;
        let extra_len = u16_at(zip, at + 30).ok_or_else(corrupt)? as usize;
        let comment_len = u16_at(zip, at + 32).ok_or_else(corrupt)? as usize;
        let header_offset = u32_at(zip, at + 42).ok_or_else(corrupt)?;

        let name_start = at + CENTRAL_LEN;
        let name_end = name_start + name_len;
        let next = name_end + extra_len + comment_len;
        if next > end {
            return Err(corrupt());
        }
        if compressed_size == u32::MAX || size == u32::MAX || header_offset == u32::MAX {
            return Err(zip64());
        }
        out.push(Entry {
            name: String::from_utf8_lossy(&zip[name_start..name_end]).into_owned(),
            method,
            flags,
            crc32,
            compressed_size: u64::from(compressed_size),
            size: u64::from(size),
            header_offset: u64::from(header_offset),
        });
        at = next;
    }
    Ok(out)
}

/// Extracts one entry, verifying its size and CRC-32.
pub fn read(zip: &[u8], entry: &Entry, max_size: u64) -> Result<Vec<u8>, String> {
    let name = &entry.name;
    let corrupt = || format!("{name}: the archive is truncated or corrupt");

    if entry.flags & FLAG_ENCRYPTED != 0 {
        return Err(format!("{name}: encrypted entries are not supported"));
    }
    if entry.size > max_size {
        return Err(format!("{name}: too large ({} bytes)", entry.size));
    }
    let at = usize::try_from(entry.header_offset).map_err(|_| corrupt())?;
    if u32_at(zip, at) != Some(LOCAL_SIGNATURE) {
        return Err(corrupt());
    }
    let name_len = u16_at(zip, at.saturating_add(26)).ok_or_else(corrupt)? as usize;
    let extra_len = u16_at(zip, at.saturating_add(28)).ok_or_else(corrupt)? as usize;
    let start = at
        .checked_add(LOCAL_LEN + name_len + extra_len)
        .ok_or_else(corrupt)?;
    let length = usize::try_from(entry.compressed_size).map_err(|_| corrupt())?;
    let end = start.checked_add(length).ok_or_else(corrupt)?;
    let data = zip.get(start..end).ok_or_else(corrupt)?;
    let size = usize::try_from(entry.size).map_err(|_| corrupt())?;

    let out = match entry.method {
        METHOD_STORED => data.to_vec(),
        // The limit caps the allocation at the size the archive promised.
        METHOD_DEFLATE => {
            miniz_oxide::inflate::decompress_to_vec_with_limit(data, size).map_err(|_| corrupt())?
        }
        other => return Err(format!("{name}: unsupported compression method {other}")),
    };
    if out.len() != size || crc32fast::hash(&out) != entry.crc32 {
        return Err(format!("{name}: failed its integrity check"));
    }
    Ok(out)
}

const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn valid_component(component: &str) -> bool {
    let allowed =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'-' | b'.' | b'(' | b')');
    let stem = component.split('.').next().unwrap_or("");
    !component.is_empty()
        && component.len() <= 64
        && component.bytes().all(allowed)
        && !component.starts_with(' ')
        && !component.ends_with(' ')
        && !component.ends_with('.')
        && component != ".."
        && !RESERVED_DEVICE_NAMES
            .iter()
            .any(|reserved| stem.trim().eq_ignore_ascii_case(reserved))
}

/// Decides whether an archive entry may be installed and, if so, where.
///
/// Only localization text is ever accepted: `Data/Localization/<language>/...`
/// ending in `.ini`, built from a conservative character set. Everything else in
/// the archive - `USER.cfg`, folders, and anything unexpected - is ignored, so
/// even a malicious archive cannot place a file outside the localization folder
/// or drop anything other than an `.ini` file.
///
/// Returns the path relative to the channel folder, using `\` separators.
pub fn install_path(entry_name: &str) -> Option<String> {
    let normalized = entry_name.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if !(4..=8).contains(&parts.len()) {
        return None;
    }
    if !parts[0].eq_ignore_ascii_case("Data") || !parts[1].eq_ignore_ascii_case("Localization") {
        return None;
    }
    if !parts.iter().all(|part| valid_component(part)) {
        return None;
    }
    let file = parts.last()?;
    if file.len() <= 4 || !file[file.len() - 4..].eq_ignore_ascii_case(".ini") {
        return None;
    }
    Some(parts.join("\\"))
}

#[cfg(test)]
#[path = "../tests/common/zipbuilder.rs"]
mod zipbuilder;

#[cfg(test)]
mod tests {
    use super::zipbuilder::ZipBuilder;
    use super::*;

    const INI: &str = "Data/Localization/english/global.ini";

    #[test]
    fn reads_stored_and_deflated_entries() {
        let text = "key=value\n".repeat(5000);
        let zip = ZipBuilder::new()
            .stored("USER.cfg", b"g_language = english")
            .dir("Data/")
            .dir("Data/Localization/")
            .deflated(INI, text.as_bytes())
            .build();
        let list = entries(&zip).unwrap();
        assert_eq!(
            list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["USER.cfg", "Data/", "Data/Localization/", INI]
        );
        assert_eq!(read(&zip, &list[0], 1024).unwrap(), b"g_language = english");
        assert_eq!(list[3].method, METHOD_DEFLATE);
        assert!(list[3].compressed_size < list[3].size);
        assert_eq!(read(&zip, &list[3], 1 << 20).unwrap(), text.as_bytes());
        assert_eq!(read(&zip, &list[1], 1024).unwrap(), b"");
    }

    #[test]
    fn empty_archive_has_no_entries() {
        assert_eq!(entries(&ZipBuilder::new().build()).unwrap(), vec![]);
    }

    #[test]
    fn tolerates_an_archive_comment() {
        let zip = ZipBuilder::new()
            .stored(INI, b"a=b")
            .comment(b"released by a robot")
            .build();
        let list = entries(&zip).unwrap();
        assert_eq!(read(&zip, &list[0], 10).unwrap(), b"a=b");
    }

    #[test]
    fn rejects_things_that_are_not_archives() {
        assert!(entries(b"").is_err());
        assert!(entries(b"<!DOCTYPE html><html><body>Not Found</body></html>").is_err());
        assert!(entries(&vec![0u8; 100_000]).is_err());
    }

    #[test]
    fn rejects_truncated_archives() {
        let zip = ZipBuilder::new()
            .deflated(INI, "key=value\n".repeat(2000).as_bytes())
            .build();
        // Cutting anywhere must fail cleanly rather than panic or misread.
        for cut in [1, 10, 21, 22, 40, 64, zip.len() / 2, zip.len() - 1] {
            let truncated = &zip[..zip.len() - cut];
            match entries(truncated) {
                Err(_) => {}
                Ok(list) => assert!(list.iter().all(|e| read(truncated, e, 1 << 20).is_err())),
            }
        }
    }

    #[test]
    fn never_panics_on_corrupted_bytes() {
        let zip = ZipBuilder::new()
            .stored("USER.cfg", b"g_language = english")
            .deflated(INI, "key=value\n".repeat(300).as_bytes())
            .build();
        // Flip every byte in turn; parsing must either fail or stay in bounds.
        for i in 0..zip.len() {
            let mut damaged = zip.clone();
            damaged[i] ^= 0xFF;
            if let Ok(list) = entries(&damaged) {
                for e in &list {
                    let _ = read(&damaged, e, 1 << 20);
                }
            }
        }
    }

    #[test]
    fn detects_content_that_does_not_match_its_crc() {
        let mut zip = ZipBuilder::new().stored(INI, b"key=value").build();
        let at = zip.windows(9).position(|w| w == b"key=value").unwrap();
        zip[at] = b'K';
        let list = entries(&zip).unwrap();
        let err = read(&zip, &list[0], 100).unwrap_err();
        assert!(err.contains("integrity"), "{err}");
    }

    #[test]
    fn enforces_the_size_limit_before_decompressing() {
        let zip = ZipBuilder::new()
            .deflated(INI, &vec![b'a'; 100_000])
            .build();
        let list = entries(&zip).unwrap();
        assert!(
            read(&zip, &list[0], 99_999)
                .unwrap_err()
                .contains("too large")
        );
        assert!(read(&zip, &list[0], 100_000).is_ok());
    }

    #[test]
    fn a_lying_size_field_cannot_inflate_past_its_claim() {
        // The entry claims 10 bytes but its deflate stream expands to 100000.
        let zip = ZipBuilder::new()
            .deflated_claiming(INI, &vec![b'a'; 100_000], 10)
            .build();
        let list = entries(&zip).unwrap();
        assert_eq!(list[0].size, 10);
        assert!(read(&zip, &list[0], 1 << 20).is_err());
    }

    #[test]
    fn rejects_encrypted_and_exotic_entries() {
        let zip = ZipBuilder::new().raw(INI, b"x", 0, 0x0001).build();
        let list = entries(&zip).unwrap();
        assert!(read(&zip, &list[0], 10).unwrap_err().contains("encrypted"));

        let zip = ZipBuilder::new().raw(INI, b"x", 12, 0).build();
        let list = entries(&zip).unwrap();
        assert!(
            read(&zip, &list[0], 10)
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn accepts_only_localization_ini_files() {
        assert_eq!(
            install_path("Data/Localization/english/global.ini").as_deref(),
            Some(r"Data\Localization\english\global.ini")
        );
        assert_eq!(
            install_path(r"data\localization\french_(france)\global.ini").as_deref(),
            Some(r"data\localization\french_(france)\global.ini")
        );
        assert_eq!(
            install_path("Data/Localization/english/sub/extra.INI").as_deref(),
            Some(r"Data\Localization\english\sub\extra.INI")
        );

        for rejected in [
            "USER.cfg",
            "readme.md",
            "Data/",
            "Data/Localization/",
            "Data/Localization/english/",
            "Data/Localization/global.ini",
            "Data/global.ini",
            "global.ini",
            "Data/Localization/english/global.txt",
            "Data/Localization/english/evil.dll",
            "Data/Localization/english/evil.exe",
            "Data/Localization/english/.ini",
            "Data/Other/english/global.ini",
            "Other/Localization/english/global.ini",
            "Bin64/Localization/english/global.ini",
        ] {
            assert_eq!(install_path(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn refuses_every_way_out_of_the_localization_folder() {
        for hostile in [
            "../Data/Localization/english/global.ini",
            "Data/Localization/../../evil.ini",
            "Data/Localization/english/../../../evil.ini",
            "Data/Localization/english/..\\..\\evil.ini",
            "Data/Localization/./english/global.ini",
            "Data//Localization/english/global.ini",
            "/Data/Localization/english/global.ini",
            "\\Data\\Localization\\english\\global.ini",
            "C:/Data/Localization/english/global.ini",
            "C:\\Windows\\System32\\Data\\Localization\\english\\global.ini",
            "\\\\server\\share\\Data\\Localization\\english\\global.ini",
            "Data/Localization/english/global.ini:hidden.ini",
            "Data/Localization/english/global.ini::$DATA",
            "Data/Localization/english/CON.ini",
            "Data/Localization/english/nul.ini",
            "Data/Localization/COM1/global.ini",
            "Data/Localization/english/global.ini ",
            "Data/Localization/english /global.ini",
            "Data/Localization/english./global.ini",
            "Data/Localization/english/global.ini.",
            "Data/Localization/english/glo\u{0}bal.ini",
            "Data/Localization/english/glo\nbal.ini",
            "Data/Localization/en*glish/global.ini",
            "Data/Localization/english/global?.ini",
            "Data/Localization/english/\u{202e}ini.lld",
            "Data/Localization/%2e%2e/global.ini",
            "Data/Localization/english/a/b/c/d/e/f/global.ini",
        ] {
            assert_eq!(install_path(hostile), None, "{hostile:?}");
        }
        let long = format!("Data/Localization/{}/global.ini", "a".repeat(65));
        assert_eq!(install_path(&long), None);
    }
}
