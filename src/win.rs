//! Small wrappers around the Win32 calls that are not specific to one feature.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, FILETIME, HANDLE, HLOCAL, LocalFree, SYSTEMTIME, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime};
use windows::core::{PCWSTR, PWSTR};

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
const DRIVE_FIXED: u32 = 3;

/// Null-terminated UTF-16 copy of `s`, for passing to Win32.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Human-readable text for a Win32/COM error, always including the code.
pub fn describe(e: &windows::core::Error) -> String {
    let message = e.message();
    let message = message.trim();
    if message.is_empty() {
        format!("error 0x{:08X}", e.code().0)
    } else {
        format!("{message} (0x{:08X})", e.code().0)
    }
}

/// 64-bit FNV-1a. Used only to derive stable, collision-unlikely names.
pub fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn local_time() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

pub fn format_time(t: &SYSTEMTIME, date_separator: char) -> String {
    format!(
        "{:04}-{:02}-{:02}{}{:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, date_separator, t.wHour, t.wMinute, t.wSecond
    )
}

/// Local time as `2026-11-12 22:41:03`.
pub fn local_timestamp() -> String {
    format_time(&local_time(), ' ')
}

/// `time` moved forward by `seconds`.
pub fn add_seconds(time: &SYSTEMTIME, seconds: u64) -> SYSTEMTIME {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    let mut file_time = FILETIME::default();
    let mut shifted = SYSTEMTIME::default();
    unsafe {
        if SystemTimeToFileTime(time, &mut file_time).is_err() {
            return *time;
        }
        let ticks = (u64::from(file_time.dwHighDateTime) << 32
            | u64::from(file_time.dwLowDateTime))
        .saturating_add(seconds.saturating_mul(TICKS_PER_SECOND));
        file_time.dwHighDateTime = (ticks >> 32) as u32;
        file_time.dwLowDateTime = ticks as u32;
        if FileTimeToSystemTime(&file_time, &mut shifted).is_err() {
            return *time;
        }
    }
    shifted
}

/// Local time `seconds` from now in the form Task Scheduler expects,
/// `2026-11-12T22:41:03`.
pub fn local_timestamp_iso_in(seconds: u64) -> String {
    format_time(&add_seconds(&local_time(), seconds), 'T')
}

/// The string SID (`S-1-5-21-...`) of the user running this process.
pub fn current_user_sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|e| describe(&e))?;

        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        // u64 storage keeps the TOKEN_USER structure correctly aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            (buffer.len() * 8) as u32,
            &mut needed,
        );
        let _ = CloseHandle(token);
        result.map_err(|e| describe(&e))?;

        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(|e| describe(&e))?;
        let sid = text.to_string().map_err(|e| e.to_string());
        let _ = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
        sid
    }
}

/// Whether `resolved` - where Windows says `intended` really lives - shows that
/// the path was silently redirected into another app's private storage.
///
/// A program started from inside a packaged app (the MSIX format used by the
/// Microsoft Store and by a growing number of desktop apps) gets a private view
/// of AppData: a folder it creates as `...\AppData\Local\Name` really ends up in
/// `...\AppData\Local\Packages\<app>\LocalCache\Local\Name`, where no other
/// program - Task Scheduler included - can see it. The program itself cannot
/// tell from the path it used; only the resolved path gives it away.
pub fn is_app_private(intended: &Path, resolved: &Path) -> bool {
    fn key(path: &Path) -> String {
        let text = path.to_string_lossy().to_ascii_lowercase();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    }
    let (intended, resolved) = (key(intended), key(resolved));
    intended != resolved
        && resolved.contains(r"\packages\")
        && resolved.contains(r"\localcache\")
        && !intended.contains(r"\localcache\")
}

/// Root paths (`C:\`) of the fixed local drives.
pub fn fixed_drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| {
            let w = wide(root);
            unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) == DRIVE_FIXED }
        })
        .collect()
}

/// Initialises COM for this thread (idempotent).
pub fn com_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    });
}

/// Ensures only one copy of the updater works on a given home folder at a time.
pub struct InstanceLock(HANDLE);

impl InstanceLock {
    /// Waits up to `wait_ms` for the lock; `None` means another copy holds it.
    pub fn acquire(name: &str, wait_ms: u32) -> Option<InstanceLock> {
        let name = wide(name);
        unsafe {
            let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr())).ok()?;
            let wait = WaitForSingleObject(handle, wait_ms);
            if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
                Some(InstanceLock(handle))
            } else {
                let _ = CloseHandle(handle);
                None
            }
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// A value for the "Installed apps" registry entry.
pub enum RegValue<'a> {
    Text(&'a str),
    Number(u32),
}

/// Writes the per-user "Installed apps" entry so the updater can be removed
/// from Windows Settings like any other program.
pub fn write_uninstall_entry(key: &str, values: &[(&str, RegValue)]) -> Result<(), String> {
    let path = wide(format!(r"{UNINSTALL_KEY}\{key}"));
    unsafe {
        let mut hkey = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("registry error {}", status.0));
        }
        let mut outcome = Ok(());
        for (name, value) in values {
            let name = wide(name);
            let status = match value {
                RegValue::Text(text) => {
                    let data = wide(text);
                    let bytes =
                        std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2);
                    RegSetValueExW(hkey, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes))
                }
                RegValue::Number(number) => RegSetValueExW(
                    hkey,
                    PCWSTR(name.as_ptr()),
                    None,
                    REG_DWORD,
                    Some(&number.to_le_bytes()),
                ),
            };
            if status != ERROR_SUCCESS {
                outcome = Err(format!("registry error {}", status.0));
                break;
            }
        }
        let _ = RegCloseKey(hkey);
        outcome
    }
}

pub fn delete_uninstall_entry(key: &str) {
    let path = wide(format!(r"{UNINSTALL_KEY}\{key}"));
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_is_null_terminated() {
        assert_eq!(wide("ab"), vec![97, 98, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn timestamps_have_the_expected_shape() {
        let t = SYSTEMTIME {
            wYear: 2026,
            wMonth: 3,
            wDay: 9,
            wHour: 7,
            wMinute: 5,
            wSecond: 1,
            ..Default::default()
        };
        assert_eq!(format_time(&t, ' '), "2026-03-09 07:05:01");
        assert_eq!(format_time(&t, 'T'), "2026-03-09T07:05:01");
    }

    #[test]
    fn adding_time_rolls_over_days_months_and_years() {
        let new_years_eve = SYSTEMTIME {
            wYear: 2026,
            wMonth: 12,
            wDay: 31,
            wHour: 23,
            wMinute: 30,
            wSecond: 15,
            ..Default::default()
        };
        assert_eq!(
            format_time(&add_seconds(&new_years_eve, 3600), 'T'),
            "2027-01-01T00:30:15"
        );
        assert_eq!(
            format_time(&add_seconds(&new_years_eve, 0), 'T'),
            "2026-12-31T23:30:15"
        );
        let leap_day_eve = SYSTEMTIME {
            wYear: 2028,
            wMonth: 2,
            wDay: 28,
            wHour: 23,
            wMinute: 59,
            wSecond: 59,
            ..Default::default()
        };
        assert_eq!(
            format_time(&add_seconds(&leap_day_eve, 1), 'T'),
            "2028-02-29T00:00:00"
        );
    }

    #[test]
    fn recognises_redirection_into_an_apps_private_storage() {
        let intended = Path::new(r"C:\Users\me\AppData\Local\StarStringsUpdater");
        // What a program started from inside a packaged app actually gets.
        assert!(is_app_private(
            intended,
            Path::new(
                r"\\?\C:\Users\me\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\StarStringsUpdater"
            )
        ));
        // The ordinary case, however Windows spells it.
        assert!(!is_app_private(
            intended,
            Path::new(r"\\?\C:\Users\me\AppData\Local\StarStringsUpdater")
        ));
        assert!(!is_app_private(
            intended,
            Path::new(r"\\?\c:\users\ME\appdata\local\starstringsupdater")
        ));
        // A relocated or linked user profile resolves elsewhere, but everyone
        // still reaches it through the same path, so it is not private.
        assert!(!is_app_private(
            intended,
            Path::new(r"\\?\D:\Profiles\me\AppData\Local\StarStringsUpdater")
        ));
        // A data folder deliberately chosen inside such a location is not a redirect.
        let chosen = Path::new(r"C:\Users\me\AppData\Local\Packages\X\LocalCache\Local\dev");
        assert!(!is_app_private(
            chosen,
            Path::new(r"\\?\C:\Users\me\AppData\Local\Packages\X\LocalCache\Local\dev")
        ));
    }

    #[test]
    fn current_user_sid_looks_like_a_sid() {
        let sid = current_user_sid().expect("sid");
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let name = format!("Local\\ssu-test-{}-{}", std::process::id(), unix_now());
        let first = InstanceLock::acquire(&name, 0).expect("first acquire");
        // A mutex is re-entrant for its owning thread, so contend from another one.
        let contender = name.clone();
        let blocked = std::thread::spawn(move || InstanceLock::acquire(&contender, 0).is_none())
            .join()
            .unwrap();
        assert!(blocked, "a second holder must be refused");
        drop(first);
        let contender = name.clone();
        let free = std::thread::spawn(move || InstanceLock::acquire(&contender, 0).is_some())
            .join()
            .unwrap();
        assert!(free, "the lock must be available again once released");
    }

    #[test]
    fn fixed_drives_include_the_system_drive() {
        let drives = fixed_drives();
        assert!(!drives.is_empty());
        assert!(drives.iter().all(|d| d.len() == 3 && d.ends_with(":\\")));
    }
}
