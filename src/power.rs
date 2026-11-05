//! Windows power mode (Settings > Power > Power mode) and battery details.
//!
//! The power mode API (power "overlay" schemes) is exported by powrprof.dll but
//! not in the SDK headers, so it is looked up at run time.

use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::core::{GUID, s, w};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Efficiency,
    Balanced,
    Performance,
}

pub const MODES: [Mode; 3] = [Mode::Efficiency, Mode::Balanced, Mode::Performance];

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Efficiency => "En iyi güç verimliliği",
            Mode::Balanced => "Dengeli",
            Mode::Performance => "En iyi performans",
        }
    }

    fn guid(self) -> GUID {
        match self {
            Mode::Efficiency => GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a),
            Mode::Balanced => GUID::zeroed(),
            Mode::Performance => GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238),
        }
    }

    pub fn from_guid(g: GUID) -> Mode {
        MODES.into_iter().find(|m| m.guid() == g).unwrap_or(Mode::Balanced)
    }
}

type GetOverlay = unsafe extern "system" fn(*mut GUID) -> u32;
type SetOverlay = unsafe extern "system" fn(*const GUID) -> u32;

fn proc<T>(name: windows::core::PCSTR) -> Option<T> {
    unsafe {
        let lib = LoadLibraryW(w!("powrprof.dll")).ok()?;
        let p = GetProcAddress(lib, name)?;
        Some(std::mem::transmute_copy(&p))
    }
}

pub fn current() -> Option<Mode> {
    let get: GetOverlay = proc(s!("PowerGetEffectiveOverlayScheme"))?;
    let mut g = GUID::zeroed();
    (unsafe { get(&mut g) } == 0).then(|| Mode::from_guid(g))
}

pub fn set(mode: Mode) -> bool {
    let Some(set) = proc::<SetOverlay>(s!("PowerSetActiveOverlayScheme")) else { return false };
    unsafe { set(&mode.guid()) == 0 }
}

/// Seconds of battery left, when Windows knows (not while charging).
pub fn remaining() -> Option<u32> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).ok()? };
    (ps.BatteryLifeTime != u32::MAX && ps.ACLineStatus != 1).then_some(ps.BatteryLifeTime)
}

/// "3 sa 12 dk kaldı", "45 dk kaldı".
pub fn format_remaining(secs: u32) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 { format!("{h} sa {m} dk kaldı") } else { format!("{m} dk kaldı") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_text() {
        assert_eq!(format_remaining(3 * 3600 + 12 * 60 + 30), "3 sa 12 dk kaldı");
        assert_eq!(format_remaining(45 * 60), "45 dk kaldı");
        assert_eq!(format_remaining(0), "0 dk kaldı");
    }

    #[test]
    fn modes_round_trip_through_guids() {
        for m in MODES {
            assert_eq!(Mode::from_guid(m.guid()), m);
        }
        assert_eq!(Mode::from_guid(GUID::from_u128(1)), Mode::Balanced, "unknown schemes read as balanced");
    }

    #[test]
    fn reads_the_current_mode() {
        assert!(current().is_some(), "powrprof overlay API missing");
    }
}
