//! Gregorian dates for the clock calendar, independent of native UI and locale.
//! Weekdays use Monday = 0; callers supply Windows' preferred first weekday.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month {
    pub year: i32,
    pub month: u8,
}

impl Date {
    pub const MIN: Self = Self {
        year: 1,
        month: 1,
        day: 1,
    };
    pub const MAX: Self = Self {
        year: 9999,
        month: 12,
        day: 31,
    };

    pub fn new(year: i32, month: u8, day: u8) -> Option<Self> {
        let length = days_in_month(year, month)?;
        (day >= 1 && day <= length).then_some(Self { year, month, day })
    }

    pub fn month(self) -> Month {
        Month {
            year: self.year,
            month: self.month,
        }
    }

    pub fn weekday(self) -> u8 {
        ((self.ordinal() - 1) % 7) as u8
    }

    /// Saturates at the calendar's supported years, even for large offsets.
    pub fn shift_days(self, days: i32) -> Self {
        let ordinal = (i64::from(self.ordinal()) + i64::from(days)).clamp(1, i64::from(Self::MAX.ordinal()));
        Self::from_ordinal(ordinal as i32).expect("clamped calendar day")
    }

    /// Moves to the requested month and clamps days such as January 31.
    pub fn shift_months(self, months: i32) -> Self {
        let month = self.month().shift_months(months);
        Self {
            year: month.year,
            month: month.month,
            day: self
                .day
                .min(days_in_month(month.year, month.month).expect("valid month")),
        }
    }

    fn ordinal(self) -> i32 {
        let previous_year = self.year - 1;
        let mut days = previous_year * 365 + previous_year / 4 - previous_year / 100 + previous_year / 400;
        for month in 1..self.month {
            days += i32::from(days_in_month(self.year, month).expect("valid month"));
        }
        days + i32::from(self.day)
    }

    fn from_ordinal(ordinal: i32) -> Option<Self> {
        if !(1..=Self::MAX.ordinal()).contains(&ordinal) {
            return None;
        }
        // Find the year in at most 14 comparisons, independent of the offset.
        let (mut low, mut high) = (1, 9999);
        while low < high {
            let middle = (low + high + 1) / 2;
            let first = Self {
                year: middle,
                month: 1,
                day: 1,
            };
            if first.ordinal() <= ordinal {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        let mut remaining = ordinal
            - Self {
                year: low,
                month: 1,
                day: 1,
            }
            .ordinal()
            + 1;
        for month in 1..=12 {
            let length = i32::from(days_in_month(low, month).expect("valid month"));
            if remaining <= length {
                return Some(Self {
                    year: low,
                    month,
                    day: remaining as u8,
                });
            }
            remaining -= length;
        }
        unreachable!("ordinal belongs to the selected year")
    }
}

impl Month {
    pub fn new(year: i32, month: u8) -> Option<Self> {
        days_in_month(year, month).map(|_| Self { year, month })
    }

    pub fn shift_months(self, months: i32) -> Self {
        let index = i64::from(self.year - 1) * 12 + i64::from(self.month - 1);
        let shifted = (index + i64::from(months)).clamp(0, 9999 * 12 - 1);
        Self {
            year: (shifted / 12) as i32 + 1,
            month: (shifted % 12) as u8 + 1,
        }
    }

    /// Includes adjacent months. Unsupported dates at the year limits are empty.
    /// First weekdays outside 0..7 are normalized to the same seven-day cycle.
    pub fn grid(self, first_weekday: u8) -> [Option<Date>; 42] {
        let first = Date {
            year: self.year,
            month: self.month,
            day: 1,
        };
        let leading = (first.weekday() + 7 - first_weekday % 7) % 7;
        let start = first.ordinal() - i32::from(leading);
        std::array::from_fn(|index| Date::from_ordinal(start + index as i32))
    }
}

pub fn days_in_month(year: i32, month: u8) -> Option<u8> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
            29
        } else {
            28
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u8, day: u8) -> Date {
        Date::new(year, month, day).unwrap()
    }

    #[test]
    fn dates_validate_gregorian_century_rules() {
        for (year, leap) in [(1900, false), (2000, true), (2024, true), (2100, false), (2400, true)] {
            assert_eq!(Date::new(year, 2, 29).is_some(), leap);
            assert_eq!(
                date(year, 2, 28).shift_days(1),
                if leap { date(year, 2, 29) } else { date(year, 3, 1) }
            );
        }
        for (year, month, day) in [
            (0, 1, 1),
            (10000, 1, 1),
            (2026, 0, 1),
            (2026, 13, 1),
            (2026, 4, 31),
            (2026, 1, 0),
        ] {
            assert_eq!(Date::new(year, month, day), None);
        }
    }

    #[test]
    fn known_weekdays_use_monday_zero() {
        for (value, weekday) in [
            (Date::MIN, 0),
            (date(1970, 1, 1), 3),
            (date(2000, 1, 1), 5),
            (date(2026, 11, 12), 3),
            (Date::MAX, 4),
        ] {
            assert_eq!(value.weekday(), weekday);
        }
    }

    #[test]
    fn grid_respects_monday_and_sunday_start_and_adjacent_months() {
        let november = Month::new(2026, 11).unwrap();
        let monday = november.grid(0);
        let sunday = november.grid(6);
        assert_eq!(monday[0], Some(date(2026, 10, 26)));
        assert_eq!(monday[6], Some(date(2026, 11, 1)));
        assert_eq!(monday[41], Some(date(2026, 12, 6)));
        assert_eq!(sunday[0], Some(date(2026, 11, 1)));
        assert_eq!(sunday[41], Some(date(2026, 12, 12)));
        for first_weekday in 0..7 {
            let grid = november.grid(first_weekday);
            for pair in grid.windows(2) {
                assert_eq!(pair[0].unwrap().shift_days(1), pair[1].unwrap());
            }
            assert_eq!(grid[0].unwrap().weekday(), first_weekday);
        }
        assert_eq!(november.grid(7), monday);
    }

    #[test]
    fn day_navigation_crosses_leap_and_year_boundaries() {
        for (before, after) in [
            (date(2025, 12, 31), date(2026, 1, 1)),
            (date(2024, 2, 28), date(2024, 2, 29)),
            (date(2024, 2, 29), date(2024, 3, 1)),
            (date(1900, 2, 28), date(1900, 3, 1)),
        ] {
            assert_eq!(before.shift_days(1), after);
            assert_eq!(after.shift_days(-1), before);
        }
    }

    #[test]
    fn month_navigation_clamps_days_and_crosses_years() {
        assert_eq!(date(2024, 1, 31).shift_months(1), date(2024, 2, 29));
        assert_eq!(date(2025, 1, 31).shift_months(1), date(2025, 2, 28));
        assert_eq!(date(2025, 12, 15).shift_months(1), date(2026, 1, 15));
        assert_eq!(date(2026, 1, 15).shift_months(-1), date(2025, 12, 15));
        assert_eq!(
            Month::new(2025, 12).unwrap().shift_months(1),
            Month::new(2026, 1).unwrap()
        );
        assert_eq!(
            Month::new(2026, 1).unwrap().shift_months(-1),
            Month::new(2025, 12).unwrap()
        );
    }

    #[test]
    fn limit_grids_leave_unsupported_dates_empty() {
        let first = Date::MIN.month().grid(6);
        assert_eq!(first[0], None);
        assert_eq!(first[1], Some(Date::MIN));
        let last = Date::MAX.month().grid(0);
        let max_index = last.iter().position(|cell| *cell == Some(Date::MAX)).unwrap();
        assert!(last[max_index + 1..].iter().all(Option::is_none));
        assert_eq!(Date::MIN.shift_days(-1), Date::MIN);
        assert_eq!(Date::MAX.shift_days(1), Date::MAX);
    }

    #[test]
    fn large_offsets_saturate_without_overflow() {
        let value = date(2026, 11, 12);
        assert_eq!(value.shift_days(i32::MIN), Date::MIN);
        assert_eq!(value.shift_days(i32::MAX), Date::MAX);
        assert_eq!(value.month().shift_months(i32::MIN), Date::MIN.month());
        assert_eq!(value.month().shift_months(i32::MAX), Date::MAX.month());
        assert_eq!(value.shift_months(i32::MIN), date(1, 1, 12));
        assert_eq!(value.shift_months(i32::MAX), date(9999, 12, 12));
    }

    #[test]
    fn ordinal_conversion_round_trips_a_full_gregorian_cycle() {
        let first = date(2000, 1, 1).ordinal();
        let last = date(2399, 12, 31).ordinal();
        for ordinal in first..=last {
            let value = Date::from_ordinal(ordinal).unwrap();
            assert_eq!(value.ordinal(), ordinal);
        }
        assert_eq!(Date::from_ordinal(1), Some(Date::MIN));
        assert_eq!(Date::from_ordinal(Date::MAX.ordinal()), Some(Date::MAX));
    }
}
