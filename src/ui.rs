//! The few dialogs this program ever shows, and console attachment.
//!
//! The executable is built for the Windows GUI subsystem so that the scheduled
//! task never flashes a console window. When it is started from a terminal it
//! attaches to that terminal instead, so its output is still visible there.

use std::ffi::c_void;
use std::path::PathBuf;

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    SetStdHandle,
};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDOK, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING,
    MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MB_YESNO, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
    MessageBoxW,
};
use windows::core::{PCWSTR, w};

use crate::logger;
use crate::win::{self, wide};

/// Whether there is somewhere for printed text to go (a terminal, pipe or file).
pub fn has_console_output() -> bool {
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null()) }
}

/// Attaches to the terminal this program was started from, if there is one and
/// output has not been redirected already.
pub fn attach_parent_console() {
    if has_console_output() {
        return;
    }
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        if let Ok(console) = CreateFileW(
            w!("CONOUT$"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, console);
            let _ = SetStdHandle(STD_ERROR_HANDLE, console);
        }
    }
}

fn message_box(title: &str, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let title = wide(title);
    let text = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            style | MB_SETFOREGROUND,
        )
    }
}

pub fn info(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONINFORMATION);
}

pub fn warn(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONWARNING);
}

pub fn error(title: &str, text: &str) {
    message_box(title, text, MB_OK | MB_ICONERROR);
}

/// OK / Cancel. Returns true for OK.
pub fn confirm(title: &str, text: &str) -> bool {
    message_box(title, text, MB_OKCANCEL | MB_ICONQUESTION) == IDOK
}

/// Yes / No with No as the default. Returns true for Yes.
pub fn ask(title: &str, text: &str) -> bool {
    message_box(title, text, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) == IDYES
}

/// Shows informational text: in the terminal if there is one, else in a dialog.
pub fn show(title: &str, text: &str) {
    if has_console_output() {
        logger::say(text);
    } else {
        info(title, text);
    }
}

/// Lets the user choose a folder. `None` if they cancelled.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    win::com_init();
    let title = wide(title);
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .ok()?;
        let _ = dialog.SetTitle(PCWSTR(title.as_ptr()));
        dialog.Show(None).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0.cast::<c_void>().cast_const()));
        path
    }
}
