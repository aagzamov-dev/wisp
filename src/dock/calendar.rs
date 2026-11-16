//! Local calendar presentation. No media, hardware, network or saved event state.
use super::calendar_model::{Date, Month};
use windows::{
    Win32::{Foundation::SYSTEMTIME, Globalization::*, System::SystemInformation::GetLocalTime},
    core::{PCWSTR, w},
};

#[derive(Clone, PartialEq, Eq)]
pub enum Action {
    Day(Date),
    Month(i32),
    Today,
}
pub struct Calendar {
    pub today: Date,
    pub selected: Date,
    pub focused: Date,
    pub month: Month,
    pub first_weekday: u8,
}
impl Calendar {
    pub fn new() -> Self {
        let today = today();
        Self {
            today,
            selected: today,
            focused: today,
            month: Month::new(today.year, today.month).expect("validated calendar month"),
            first_weekday: first_weekday(),
        }
    }
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::Day(day) => {
                self.selected = day;
                self.focused = day;
                self.month = day.month();
            }
            Action::Month(delta) => {
                self.month = self.month.shift_months(delta);
                // Browse from the viewed month even if Tab focused an adjacent
                // grid date; clamp its day only in the final target month.
                let months = (self.month.year - self.focused.year) * 12 + i32::from(self.month.month)
                    - i32::from(self.focused.month);
                self.focused = self.focused.shift_months(months);
            }
            Action::Today => {
                self.selected = self.today;
                self.focused = self.today;
                self.month = self.today.month();
            }
        }
    }
    pub fn move_days(&mut self, days: i32) {
        self.focused = self.focused.shift_days(days);
        self.month = self.focused.month();
    }
    fn refresh_date(&mut self, date: Date, first: u8) -> bool {
        let following = self.selected == self.today && self.focused == self.today && self.month == self.today.month();
        let changed = date != self.today || first != self.first_weekday;
        self.today = date;
        self.first_weekday = first;
        if following {
            self.apply(Action::Today);
        }
        changed
    }
    pub fn refresh(&mut self) -> bool {
        self.refresh_date(today(), first_weekday())
    }
}
pub fn today() -> Date {
    let now = unsafe { GetLocalTime() };
    Date::new(now.wYear as i32, now.wMonth as u8, now.wDay as u8).unwrap_or(Date::MIN)
}
fn decode(buffer: &[u16], len: i32) -> String {
    String::from_utf16_lossy(&buffer[..len.saturating_sub(1).max(0) as usize])
}
fn calendar_text(kind: u32) -> String {
    let mut buffer = [0u16; 128];
    let len = unsafe {
        GetCalendarInfoEx(
            PCWSTR::null(),
            CAL_GREGORIAN,
            PCWSTR::null(),
            kind,
            Some(&mut buffer),
            None,
        )
    };
    decode(&buffer, len)
}
pub fn month_label(month: Month) -> String {
    format!(
        "{} {}",
        calendar_text(CAL_SMONTHNAME1 + u32::from(month.month) - 1),
        month.year
    )
}
pub fn weekday_label(index: u8) -> String {
    calendar_text(CAL_SSHORTESTDAYNAME1 + u32::from(index % 7))
}
pub fn first_weekday() -> u8 {
    let mut buffer = [0u16; 8];
    let len = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_IFIRSTDAYOFWEEK, Some(&mut buffer)) };
    decode(&buffer, len).parse::<u8>().unwrap_or(0).min(6)
}
pub fn long_label(date: Date) -> String {
    let now = SYSTEMTIME {
        wYear: date.year as u16,
        wMonth: date.month as u16,
        wDay: date.day as u16,
        ..Default::default()
    };
    let mut buffer = [0u16; 256];
    let len = unsafe {
        GetDateFormatEx(
            PCWSTR::null(),
            DATE_LONGDATE,
            Some(&now),
            PCWSTR::null(),
            Some(&mut buffer),
            PCWSTR::null(),
        )
    };
    localized_or_numeric(decode(&buffer, len), date, false)
}
pub fn day_label(date: Date) -> String {
    let mut buffer = [0u16; 32];
    let now = SYSTEMTIME {
        wYear: date.year as u16,
        wMonth: date.month as u16,
        wDay: date.day as u16,
        ..Default::default()
    };
    let len = unsafe {
        GetDateFormatEx(
            PCWSTR::null(),
            Default::default(),
            Some(&now),
            w!("d"),
            Some(&mut buffer),
            PCWSTR::null(),
        )
    };
    localized_or_numeric(decode(&buffer, len), date, true)
}

fn localized_or_numeric(label: String, date: Date, day_only: bool) -> String {
    if !label.is_empty() {
        label
    } else if day_only {
        date.day.to_string()
    } else {
        format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_nls_years_keep_visible_day_and_selected_date() {
        let date = Date::new(1, 1, 1).unwrap();
        assert_eq!(localized_or_numeric(String::new(), date, true), "1");
        assert_eq!(localized_or_numeric(String::new(), date, false), "0001-01-01");
        assert_eq!(
            localized_or_numeric("localized label".into(), date, false),
            "localized label"
        );
    }
    fn calendar(date: Date) -> Calendar {
        Calendar {
            today: date,
            selected: date,
            focused: date,
            month: date.month(),
            first_weekday: 0,
        }
    }
    #[test]
    fn browsing_keeps_selection_and_full_today_identity() {
        let today = Date::new(2026, 11, 12).unwrap();
        let mut c = calendar(today);
        c.apply(Action::Month(-22));
        assert_eq!(c.month, Month::new(2025, 1).unwrap());
        assert_eq!(c.selected, today);
        assert_eq!(c.today, today);
        assert_eq!(c.focused, Date::new(2025, 1, 12).unwrap());
        assert!(
            c.month
                .grid(0)
                .into_iter()
                .flatten()
                .all(|date| date != c.today && date != c.selected)
        );
        c.move_days(1);
        assert_eq!(c.focused, Date::new(2025, 1, 13).unwrap());
        assert_eq!(c.selected, today);
        c.apply(Action::Day(c.focused));
        assert_eq!(c.selected, Date::new(2025, 1, 13).unwrap());
        c.apply(Action::Today);
        assert_eq!((c.selected, c.focused, c.month), (today, today, today.month()));
    }
    #[test]
    fn browse_leap_boundaries_and_adjacent_selection() {
        let date = Date::new(2024, 1, 31).unwrap();
        let mut c = calendar(date);
        c.apply(Action::Month(1));
        assert_eq!(c.focused, Date::new(2024, 2, 29).unwrap());
        assert_eq!(c.selected, date);
        c.move_days(1);
        assert_eq!(c.focused, Date::new(2024, 3, 1).unwrap());
        c.apply(Action::Day(Date::new(2024, 2, 29).unwrap()));
        assert_eq!(c.month, Month::new(2024, 2).unwrap());
        c.apply(Action::Month(i32::MAX));
        assert_eq!(c.month, Month::new(9999, 12).unwrap());
        c.apply(Action::Month(i32::MIN));
        assert_eq!(c.month, Month::new(1, 1).unwrap());
        c.move_days(i32::MIN);
        assert_eq!(c.focused, Date::MIN);
    }
    #[test]
    fn adjacent_focus_clamps_only_in_target_month() {
        let today = Date::new(2024, 2, 1).unwrap();
        let mut c = calendar(today);
        c.focused = Date::new(2024, 1, 31).unwrap();
        c.apply(Action::Month(1));
        assert_eq!(c.month, Month::new(2024, 3).unwrap());
        assert_eq!(c.focused, Date::new(2024, 3, 31).unwrap());
        assert_eq!(c.selected, today);
    }
    #[test]
    fn midnight_updates_today_without_stealing_browsed_month() {
        let old = Date::new(2026, 12, 31).unwrap();
        let new = Date::new(2027, 1, 1).unwrap();
        let mut c = calendar(old);
        assert!(c.refresh_date(new, 0));
        assert_eq!((c.selected, c.focused, c.month), (new, new, new.month()));
        c.apply(Action::Month(-12));
        let view = c.month;
        let selection = c.selected;
        let focus = c.focused;
        assert!(c.refresh_date(Date::new(2027, 1, 2).unwrap(), 6));
        assert_eq!((c.selected, c.focused, c.month), (selection, focus, view));
        assert_eq!(c.first_weekday, 6);
    }
}
