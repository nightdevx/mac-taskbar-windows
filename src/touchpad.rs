//! Three-finger horizontal swipes on a precision touchpad, read as raw HID
//! input while Launchpad is open (Windows sends no message for them).

use std::collections::HashMap;

use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use windows::Win32::UI::Input::*;

const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_DESKTOP: u16 = 0x01;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_X: u16 = 0x30;
const USAGE_TIP: u16 = 0x42;
const USAGE_CONTACT_ID: u16 = 0x51;
const USAGE_CONTACT_COUNT: u16 = 0x54;

/// Starts (or stops) raw touchpad input to `hwnd`.
pub fn listen(hwnd: HWND, on: bool) {
    let dev = RAWINPUTDEVICE {
        usUsagePage: PAGE_DIGITIZER,
        usUsage: USAGE_TOUCHPAD,
        dwFlags: if on { RAWINPUTDEVICE_FLAGS(0) } else { RIDEV_REMOVE },
        hwndTarget: if on { hwnd } else { HWND::default() },
    };
    if let Err(e) = unsafe { RegisterRawInputDevices(&[dev], size_of::<RAWINPUTDEVICE>() as u32) } {
        crate::log::error(&format!("touchpad raw input: {e}"));
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    None,
    /// Fingers moved by this fraction of the touchpad width (negative = left).
    Move(f32),
    /// Fewer than three fingers now: the swipe is over.
    End,
}

/// Turns contact reports into swipe events. Precision touchpads send either
/// all contacts in one report or one per report ("hybrid"); the contact count
/// is only set in the first report of a frame.
#[derive(Default)]
pub struct Gesture {
    fingers: u32,
    xs: HashMap<u32, f32>,
    active: bool,
}

impl Gesture {
    /// `count`: contact count usage (0 = same frame as before).
    /// `contacts`: (contact id, x 0..1, tip down).
    pub fn report(&mut self, count: u32, contacts: &[(u32, f32, bool)]) -> Event {
        if count > 0 {
            self.fingers = count;
        }
        let (mut sum, mut n) = (0.0, 0);
        for &(id, x, tip) in contacts {
            if tip {
                if let Some(old) = self.xs.insert(id, x) {
                    sum += x - old;
                    n += 1;
                }
            } else {
                self.xs.remove(&id);
            }
        }
        if self.fingers == 3 && self.xs.len() == 3 {
            self.active = true;
            return if n > 0 { Event::Move(sum / n as f32) } else { Event::None };
        }
        if self.active {
            self.active = false;
            return Event::End;
        }
        Event::None
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

/// HID layout of one touchpad.
struct Device {
    preparsed: Vec<u8>,
    /// Link collections that carry a finger's X (one per contact slot).
    fingers: Vec<(u16, f32)>,
    count_link: Option<u16>,
}

impl Device {
    fn read(handle: HANDLE) -> Option<Device> {
        unsafe {
            let mut size = 0u32;
            GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut size);
            if size == 0 {
                return None;
            }
            let mut preparsed = vec![0u8; size as usize];
            if GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, Some(preparsed.as_mut_ptr() as *mut _), &mut size) == u32::MAX {
                return None;
            }
            let pp = PHIDP_PREPARSED_DATA(preparsed.as_ptr() as isize);
            let mut caps = HIDP_CAPS::default();
            if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
                return None;
            }
            let mut n = caps.NumberInputValueCaps;
            let mut values = vec![HIDP_VALUE_CAPS::default(); n as usize];
            if HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut n, pp) != HIDP_STATUS_SUCCESS {
                return None;
            }
            let usage = |v: &HIDP_VALUE_CAPS| if v.IsRange { v.Anonymous.Range.UsageMin } else { v.Anonymous.NotRange.Usage };
            let fingers = values[..n as usize]
                .iter()
                .filter(|v| v.UsagePage == PAGE_DESKTOP && usage(v) == USAGE_X && v.LogicalMax > 0)
                .map(|v| (v.LinkCollection, v.LogicalMax as f32))
                .collect::<Vec<_>>();
            let count_link = values[..n as usize]
                .iter()
                .find(|v| v.UsagePage == PAGE_DIGITIZER && usage(v) == USAGE_CONTACT_COUNT)
                .map(|v| v.LinkCollection);
            (!fingers.is_empty()).then_some(Device { preparsed, fingers, count_link })
        }
    }

    fn contacts(&self, report: &mut [u8]) -> (u32, Vec<(u32, f32, bool)>) {
        let pp = PHIDP_PREPARSED_DATA(self.preparsed.as_ptr() as isize);
        let value = |page, link, usage, report: &[u8]| unsafe {
            let mut v = 0u32;
            (HidP_GetUsageValue(HidP_Input, page, Some(link), usage, &mut v, pp, report) == HIDP_STATUS_SUCCESS).then_some(v)
        };
        let count = self.count_link.and_then(|l| value(PAGE_DIGITIZER, l, USAGE_CONTACT_COUNT, report)).unwrap_or(0);
        let mut out = Vec::new();
        for &(link, max) in &self.fingers {
            let Some(x) = value(PAGE_DESKTOP, link, USAGE_X, report) else { continue };
            let id = value(PAGE_DIGITIZER, link, USAGE_CONTACT_ID, report).unwrap_or(link as u32);
            let mut usages = [0u16; 16];
            let mut len = usages.len() as u32;
            let ok = unsafe { HidP_GetUsages(HidP_Input, PAGE_DIGITIZER, Some(link), usages.as_mut_ptr(), &mut len, pp, report) };
            let tip = ok == HIDP_STATUS_SUCCESS && usages[..len as usize].contains(&USAGE_TIP);
            out.push((id, x as f32 / max, tip));
        }
        (count, out)
    }
}

/// Raw input state for the window that listens.
#[derive(Default)]
pub struct Swipe {
    gesture: Gesture,
    devices: HashMap<isize, Option<Device>>,
}

impl Swipe {
    /// Handles one `WM_INPUT`.
    pub fn feed(&mut self, lp: LPARAM) -> Event {
        let raw = HRAWINPUT(lp.0 as *mut _);
        let header = size_of::<RAWINPUTHEADER>() as u32;
        let mut size = 0u32;
        unsafe { GetRawInputData(raw, RID_INPUT, None, &mut size, header) };
        if size == 0 {
            return Event::None;
        }
        // u64 backing keeps the RAWINPUT header aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        if unsafe { GetRawInputData(raw, RID_INPUT, Some(buf.as_mut_ptr() as *mut _), &mut size, header) } == u32::MAX {
            return Event::None;
        }
        let input = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
        if input.header.dwType != RIM_TYPEHID.0 {
            return Event::None;
        }
        let device = self.devices.entry(input.header.hDevice.0 as isize).or_insert_with(|| Device::read(input.header.hDevice));
        let Some(device) = device else { return Event::None };
        let hid = unsafe { input.data.hid };
        let (each, reports) = (hid.dwSizeHid as usize, hid.dwCount as usize);
        let base = unsafe { (&input.data.hid.bRawData as *const u8).offset_from(buf.as_ptr() as *const u8) } as usize;
        let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let mut last = Event::None;
        let mut moved = 0.0;
        for r in 0..reports {
            let (start, end) = (base + r * each, base + (r + 1) * each);
            if end > bytes.len() {
                break;
            }
            let (count, contacts) = device.contacts(&mut bytes[start..end]);
            match self.gesture.report(count, &contacts) {
                Event::Move(dx) => {
                    moved += dx;
                    last = Event::Move(moved);
                }
                Event::End => last = Event::End,
                Event::None => {}
            }
        }
        last
    }

    pub fn active(&self) -> bool {
        self.gesture.active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_fingers_move_together() {
        let mut g = Gesture::default();
        assert_eq!(g.report(3, &[(1, 0.5, true), (2, 0.6, true), (3, 0.7, true)]), Event::None);
        assert!(g.active());
        match g.report(3, &[(1, 0.4, true), (2, 0.5, true), (3, 0.6, true)]) {
            Event::Move(dx) => assert!((dx + 0.1).abs() < 1e-6),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn hybrid_reports_one_contact_each() {
        let mut g = Gesture::default();
        g.report(3, &[(1, 0.5, true)]);
        g.report(0, &[(2, 0.6, true)]);
        g.report(0, &[(3, 0.7, true)]);
        assert!(g.active());
        match g.report(3, &[(1, 0.6, true)]) {
            Event::Move(dx) => assert!((dx - 0.1).abs() < 1e-6),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn lifting_a_finger_ends_the_swipe() {
        let mut g = Gesture::default();
        g.report(3, &[(1, 0.5, true), (2, 0.6, true), (3, 0.7, true)]);
        assert_eq!(g.report(3, &[(1, 0.5, true), (2, 0.6, true), (3, 0.7, false)]), Event::End);
        assert!(!g.active());
        assert_eq!(g.report(2, &[(1, 0.5, true), (2, 0.6, true)]), Event::None);
    }

    #[test]
    fn two_fingers_never_swipe() {
        let mut g = Gesture::default();
        assert_eq!(g.report(2, &[(1, 0.5, true), (2, 0.6, true)]), Event::None);
        assert_eq!(g.report(2, &[(1, 0.2, true), (2, 0.3, true)]), Event::None);
    }
}
