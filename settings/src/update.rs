//! Entry points shared by About and the silent updater started by Wisp.
pub mod feed;
pub mod install;

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
use windows::core::PCWSTR;

const CHECK_INTERVAL: Duration = Duration::from_hours(24);

pub fn dispatch() -> bool {
    if install::dispatch() {
        return true;
    }
    if !std::env::args_os().skip(1).eq(["--update-background"]) {
        return false;
    }
    if let Err(error) = background() {
        crate::logging::error("update.background", format_args!("{error}"));
    }
    true
}

fn background() -> Result<(), String> {
    if !feed::available() {
        return Ok(());
    }
    let path = crate::config::path()?;
    if !background_enabled()? || !install::background_allowed()? {
        return Ok(());
    }
    let marker = path.with_file_name("update-last-check");
    if !reserve_check(&marker, SystemTime::now()).map_err(|error| format!("Cannot record update check: {error}"))? {
        return Ok(());
    }
    let release = feed::check().map_err(|error| format!("Check for updates: {error}"))?;
    let Some(release) = release else {
        crate::logging::event("update.background", format_args!("outcome=up-to-date"));
        return Ok(());
    };
    if background_enabled()? {
        install::schedule(&release, false).map_err(|error| format!("Schedule update: {error}"))?;
        crate::logging::event(
            "update.background",
            format_args!("outcome=scheduled version={}", release.version),
        );
    }
    Ok(())
}

pub(super) fn background_enabled() -> Result<bool, String> {
    // The surface exists only while Wisp owns its active launcher. Its
    // lifecycle listener can be present while Wisp is paused or retiring.
    let store = crate::config::Store::new(crate::config::path()?);
    Ok(store.load()?.automatic_updates
        && unsafe { FindWindowW(crate::channel::LAUNCHER_SURFACE_CLASS, PCWSTR::null()) }.is_ok())
}

fn reserve_check(path: &Path, now: SystemTime) -> io::Result<bool> {
    let mut options = OpenOptions::new();
    // A second helper cannot race between inspecting and refreshing the time.
    options.read(true).write(true).share_mode(0);
    let file = match options.clone().create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let file = options.open(path)?;
            let last_check = file.metadata()?.modified()?;
            if now.duration_since(last_check).unwrap_or_default() < CHECK_INTERVAL {
                return Ok(false);
            }
            file
        }
        Err(error) => return Err(error),
    };
    file.set_modified(now)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_respects_the_last_attempt_until_the_next_day() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("update-last-check");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(reserve_check(&marker, now).unwrap());
        assert!(!reserve_check(&marker, now + Duration::from_hours(1)).unwrap());
        assert!(reserve_check(&marker, now + CHECK_INTERVAL).unwrap());
        assert!(!reserve_check(&marker, now + CHECK_INTERVAL).unwrap());
    }
}
