//! The Windows scheduled task that stands in for a background process.
//!
//! Task Scheduler is already running on every Windows machine. Registering a
//! task with it means this program only exists as a process for the moment it
//! takes to do a check; between checks nothing of it is loaded at all.

use std::path::Path;

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN, TaskScheduler,
};
use windows::Win32::System::Variant::{VARIANT, VariantTimeToSystemTime};
use windows::core::BSTR;

use crate::win;

/// How often the task runs, as an ISO 8601 duration and in seconds.
pub const INTERVAL: &str = "PT1H";
pub const INTERVAL_SECONDS: u64 = 60 * 60;
/// How long after sign-in the first check runs, giving the network time to come up.
pub const LOGON_DELAY: &str = "PT1M";

const DESCRIPTION: &str = "Checks whether a new release of MrKraken's StarStrings is available and \
installs it into the Star Citizen folder. Runs for about a second at sign-in and once an hour; \
nothing stays running in between.";

pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The task definition.
///
/// * Runs as the signed-in user with ordinary rights - no elevation, no stored
///   password. The game folder is writable by that user.
/// * Two triggers: shortly after sign-in, and once an hour. A check missed
///   because the PC was off or asleep is made up as soon as possible.
/// * Low priority, never wakes the PC, and a new check never overlaps an old one.
pub fn definition(exe: &Path, user_sid: &str, start: &str) -> String {
    let command = xml_escape(&format!("\"{}\"", exe.display()));
    let user = xml_escape(user_sid);
    let start = xml_escape(start);
    let description = xml_escape(DESCRIPTION);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>StarStrings Updater</Author>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>{LOGON_DELAY}</Delay>
    </LogonTrigger>
    <TimeTrigger>
      <Repetition>
        <Interval>{INTERVAL}</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>{start}</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT10M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>run</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn root_folder() -> Result<ITaskFolder, String> {
    win::com_init();
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| win::describe(&e))?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .map_err(|e| win::describe(&e))?;
        service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| win::describe(&e))
    }
}

/// Creates the task, or replaces it if it already exists.
pub fn register(name: &str, definition: &str) -> Result<(), String> {
    let folder = root_folder()?;
    let empty = VARIANT::default();
    unsafe {
        folder
            .RegisterTask(
                &BSTR::from(name),
                &BSTR::from(definition),
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )
            .map(|_| ())
            .map_err(|e| win::describe(&e))
    }
}

/// Removes the task. Returns whether there was one to remove.
pub fn unregister(name: &str) -> Result<bool, String> {
    let folder = root_folder()?;
    unsafe {
        if folder.GetTask(&BSTR::from(name)).is_err() {
            return Ok(false);
        }
        folder
            .DeleteTask(&BSTR::from(name), 0)
            .map(|_| true)
            .map_err(|e| win::describe(&e))
    }
}

pub struct TaskInfo {
    pub enabled: bool,
    /// Exit code of the last run (0 = success).
    pub last_result: i32,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
}

/// Task Scheduler reports "never" as a date in 1899 or 1999.
fn ole_date(value: f64) -> Option<String> {
    let mut time = SYSTEMTIME::default();
    let converted = unsafe { VariantTimeToSystemTime(value, &mut time) };
    (converted != 0 && time.wYear > 2000).then(|| win::format_time(&time, ' '))
}

/// Details of the registered task, or `None` if it is not registered.
pub fn query(name: &str) -> Option<TaskInfo> {
    let folder = root_folder().ok()?;
    unsafe {
        let task = folder.GetTask(&BSTR::from(name)).ok()?;
        Some(TaskInfo {
            enabled: task.Enabled().map(|b| b.as_bool()).unwrap_or(true),
            last_result: task.LastTaskResult().unwrap_or(0),
            last_run: task.LastRunTime().ok().and_then(ole_date),
            next_run: task.NextRunTime().ok().and_then(ole_date),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml() {
        assert_eq!(
            xml_escape(r#"C:\Users\Tom & "Jerry" <dev>\it's"#),
            r"C:\Users\Tom &amp; &quot;Jerry&quot; &lt;dev&gt;\it&apos;s"
        );
        assert_eq!(xml_escape("a\u{0}b\nc"), "abc");
    }

    #[test]
    fn definition_runs_the_given_exe_for_the_given_user() {
        let xml = definition(
            Path::new(
                r"C:\Users\Tom & Jerry\AppData\Local\StarStringsUpdater\starstrings-updater.exe",
            ),
            "S-1-5-21-1-2-3-1001",
            "2026-11-12T22:41:03",
        );
        // The path is quoted (it may contain spaces) and escaped.
        assert!(xml.contains(
            r"<Command>&quot;C:\Users\Tom &amp; Jerry\AppData\Local\StarStringsUpdater\starstrings-updater.exe&quot;</Command>"
        ));
        assert!(xml.contains("<Arguments>run</Arguments>"));
        assert_eq!(
            xml.matches("<UserId>S-1-5-21-1-2-3-1001</UserId>").count(),
            2
        );
        assert!(xml.contains("<StartBoundary>2026-11-12T22:41:03</StartBoundary>"));
    }

    #[test]
    fn definition_keeps_the_task_unobtrusive() {
        let xml = definition(Path::new(r"C:\x.exe"), "S-1-5-18", "2026-01-01T00:00:00");
        for expected in [
            "<LogonTrigger>",
            "<Interval>PT1H</Interval>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            "<WakeToRun>false</WakeToRun>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<Hidden>false</Hidden>",
            "<Priority>7</Priority>",
        ] {
            assert!(xml.contains(expected), "missing {expected}");
        }
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("<Password>"));
    }

    #[test]
    fn never_run_dates_are_reported_as_none() {
        assert_eq!(ole_date(0.0), None);
        // 30 November 1999, what Task Scheduler returns for "never run".
        assert_eq!(ole_date(36494.0), None);
        // 12 November 2026, 12:00.
        assert_eq!(ole_date(46338.5).as_deref(), Some("2026-11-12 12:00:00"));
    }

    #[test]
    fn querying_a_task_that_does_not_exist_is_not_an_error() {
        assert!(query("StarStrings Updater - no such task (unit test)").is_none());
        assert_eq!(
            unregister("StarStrings Updater - no such task (unit test)"),
            Ok(false)
        );
    }
}
