//! Locale-aware menu bar text (clock, percentages) and the clock's wake-up schedule.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetLocaleInfoEx, GetTimeFormatEx, LOCALE_IPOSITIVEPERCENT, LOCALE_RETURN_NUMBER, TIME_NOSECONDS,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::{PCWSTR, w};

/// "Per 5 Kas 14:32" on a Turkish system; other locales get their own names.
pub fn text(now: &SYSTEMTIME) -> String {
    let mut date = [0u16; 64];
    let mut time = [0u16; 32];
    unsafe {
        let d = GetDateFormatEx(PCWSTR::null(), ENUM_DATE_FORMATS_FLAGS(0), Some(now), w!("ddd d MMM"), Some(&mut date), PCWSTR::null());
        let t = GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(now), PCWSTR::null(), Some(&mut time));
        let part = |buf: &[u16], n: i32| String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize]);
        format!("{} {}", part(&date, d), part(&time, t))
    }
}

/// Percentage in the user's locale: "%72" in Turkish, "72%" in English.
pub fn percent(n: u8) -> String {
    let mut buf = [0u16; 2]; // LOCALE_RETURN_NUMBER writes one u32
    let ok = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_IPOSITIVEPERCENT | LOCALE_RETURN_NUMBER, Some(&mut buf)) } > 0;
    let pattern = if ok { buf[0] as u32 | (buf[1] as u32) << 16 } else { 1 };
    format_percent(n, pattern)
}

/// `LOCALE_IPOSITIVEPERCENT` patterns: 0 "# %", 1 "#%", 2 "%#", 3 "% #".
pub fn format_percent(n: u8, pattern: u32) -> String {
    match pattern {
        0 => format!("{n} %"),
        2 => format!("%{n}"),
        3 => format!("% {n}"),
        _ => format!("{n}%"),
    }
}

pub fn now() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

/// Milliseconds until the next minute starts. Never 0, so a timer always waits.
pub fn ms_to_next_minute(second: u16, millis: u16) -> u32 {
    60_000 - (second as u32 * 1000 + millis as u32).min(59_999)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_minute_delay() {
        assert_eq!(ms_to_next_minute(0, 0), 60_000);
        assert_eq!(ms_to_next_minute(30, 0), 30_000);
        assert_eq!(ms_to_next_minute(59, 999), 1);
        // Leap second (second = 60) must not underflow.
        assert_eq!(ms_to_next_minute(60, 500), 1);
    }

    #[test]
    fn percent_patterns() {
        assert_eq!(format_percent(72, 0), "72 %");
        assert_eq!(format_percent(72, 1), "72%");
        assert_eq!(format_percent(100, 2), "%100");
        assert_eq!(format_percent(5, 3), "% 5");
        assert_eq!(format_percent(5, 99), "5%");
    }

    #[test]
    fn percent_uses_a_locale_pattern() {
        let s = percent(42);
        assert!(s.contains("42") && s.contains('%'), "{s}");
    }

    #[test]
    fn text_has_date_and_time() {
        let st = SYSTEMTIME { wYear: 2026, wMonth: 11, wDay: 5, wDayOfWeek: 4, wHour: 14, wMinute: 32, ..Default::default() };
        let s = text(&st);
        assert!(s.contains('5'), "{s}");
        assert!(s.contains("32"), "{s}");
        assert!(!s.contains('\0'), "{s}");
        assert!(!s.contains("  "), "{s}");
    }
}
