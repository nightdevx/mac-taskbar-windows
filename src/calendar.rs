//! Month grid for the menu bar calendar, in the user's locale.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK,
    LOCALE_RETURN_NUMBER,
};
use windows::core::{HSTRING, PCWSTR};

pub fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Day of week, 0 = Monday … 6 = Sunday (Sakamoto's method).
pub fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let sunday_first = (y + y / 4 - y / 100 + y / 400 + T[month as usize - 1] + day as i32).rem_euclid(7);
    ((sunday_first + 6) % 7) as u32
}

/// Six weeks of day numbers; `first` is the locale's first weekday (0 = Monday).
pub fn grid(year: i32, month: u32, first: u32) -> [[Option<u8>; 7]; 6] {
    let mut g = [[None; 7]; 6];
    let lead = (weekday(year, month, 1) + 7 - first) % 7;
    for d in 1..=days_in_month(year, month) {
        let i = (lead + d - 1) as usize;
        g[i / 7][i % 7] = Some(d as u8);
    }
    g
}

/// Month `delta` months away from (year, month).
pub fn shift(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let m = year * 12 + month as i32 - 1 + delta;
    (m.div_euclid(12), (m.rem_euclid(12) + 1) as u32)
}

/// Locale's first day of the week (0 = Monday), Monday when unknown.
pub fn first_weekday() -> u32 {
    let mut buf = [0u16; 2];
    let ok = unsafe {
        GetLocaleInfoEx(PCWSTR::null(), LOCALE_IFIRSTDAYOFWEEK | LOCALE_RETURN_NUMBER, Some(&mut buf))
    } > 0;
    if ok { (buf[0] as u32).min(6) } else { 0 }
}

fn format(year: i32, month: u32, day: u32, pattern: &str) -> String {
    let st = SYSTEMTIME { wYear: year as u16, wMonth: month as u16, wDay: day as u16, ..Default::default() };
    let mut buf = [0u16; 64];
    let n = unsafe {
        GetDateFormatEx(PCWSTR::null(), ENUM_DATE_FORMATS_FLAGS(0), Some(&st), &HSTRING::from(pattern), Some(&mut buf), PCWSTR::null())
    };
    String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize])
}

/// "Kasım 2026" on a Turkish system.
pub fn title(year: i32, month: u32) -> String {
    format(year, month, 1, "MMMM yyyy")
}

/// Short weekday names starting at `first` ("Pzt", "Sal", …).
pub fn weekday_names(first: u32) -> Vec<String> {
    // 2024-01-01 was a Monday.
    (0..7).map(|i| format(2024, 1, 1 + (first + i) % 7, "ddd")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays_of_known_dates() {
        assert_eq!(weekday(2024, 1, 1), 0, "Monday");
        assert_eq!(weekday(2026, 11, 5), 3, "Thursday");
        assert_eq!(weekday(2000, 2, 29), 1, "Tuesday");
    }

    #[test]
    fn month_lengths_and_leap_years() {
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(2026, 11), 30);
    }

    #[test]
    fn grid_places_first_day_by_locale() {
        // November 2026 starts on a Sunday.
        let monday_first = grid(2026, 11, 0);
        assert_eq!(monday_first[0][6], Some(1));
        assert_eq!(monday_first[0][5], None);
        let sunday_first = grid(2026, 11, 6);
        assert_eq!(sunday_first[0][0], Some(1));
        let last = monday_first.iter().flatten().flatten().max();
        assert_eq!(last, Some(&30));
    }

    #[test]
    fn shifting_months_crosses_years() {
        assert_eq!(shift(2026, 1, -1), (2025, 12));
        assert_eq!(shift(2026, 12, 1), (2027, 1));
        assert_eq!(shift(2026, 11, 0), (2026, 11));
        assert_eq!(shift(2026, 3, -15), (2024, 12));
    }
}
