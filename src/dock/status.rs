//! The volume, battery, clock and date at the right end of the dock.

use windows::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, TIME_NOSECONDS};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::{PCWSTR, w};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// In the user's time format, such as "1:45 PM" or "13:45".
    pub time: String,
    /// "Fri 30 Oct".
    pub date: String,
    pub battery: Option<Battery>,
    /// `None` without an output device.
    pub volume: Option<Volume>,
    /// Changes once a minute, so the dock knows when to redraw.
    pub minute: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    /// 0 to 100.
    pub percent: u32,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// 0 to 100.
    pub percent: u32,
    pub charging: bool,
}

pub fn read() -> Status {
    let now = unsafe { GetLocalTime() };
    let mut buffer = [0u16; 64];
    let text = |len: i32, buffer: &[u16]| String::from_utf16_lossy(&buffer[..(len.max(1) - 1) as usize]);
    let time = unsafe {
        let len = GetTimeFormatEx(
            PCWSTR::null(),
            TIME_NOSECONDS,
            Some(&now),
            PCWSTR::null(),
            Some(&mut buffer),
        );
        text(len, &buffer)
    };
    let date = unsafe {
        let len = GetDateFormatEx(
            PCWSTR::null(),
            Default::default(),
            Some(&now),
            w!("ddd d MMM"),
            Some(&mut buffer),
            PCWSTR::null(),
        );
        text(len, &buffer)
    };
    Status {
        time,
        date,
        battery: battery(),
        volume: crate::commands::audio::level().map(|(percent, muted)| Volume { percent, muted }),
        minute: now.wMinute,
    }
}

impl Status {
    /// `tray` is the number of inline notification icons, if the dock hosts them.
    pub fn parts(&self, tray: Option<usize>) -> Vec<super::logic::Part> {
        super::logic::parts(tray, true, self.battery.is_some())
    }
}

fn battery() -> Option<Battery> {
    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut status).ok()? };
    // 128 means no system battery; 255 means the charge is unknown.
    if status.BatteryFlag & 128 != 0 || status.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery {
        percent: status.BatteryLifePercent as u32,
        charging: status.ACLineStatus == 1,
    })
}
