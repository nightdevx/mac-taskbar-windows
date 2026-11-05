//! Screen-edge reservation through the shell's AppBar API, so maximised
//! windows stop at our bars instead of sliding under them.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::{ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA, SHAppBarMessage};

/// Rect for a bar of `height` px along the top of `monitor`.
pub fn top_rect(monitor: RECT, height: i32) -> RECT {
    RECT { bottom: monitor.top + height, ..monitor }
}

fn data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: hwnd, ..Default::default() }
}

pub fn register(hwnd: HWND, callback_msg: u32) {
    let mut d = APPBARDATA { uCallbackMessage: callback_msg, ..data(hwnd) };
    unsafe { SHAppBarMessage(ABM_NEW, &mut d) };
}

/// Asks the shell for the top edge of `monitor`, returns the rect it granted.
/// Another top bar (for example a moved Windows taskbar) can push ours down.
pub fn set_top(hwnd: HWND, monitor: RECT, height: i32) -> RECT {
    let mut d = APPBARDATA { uEdge: ABE_TOP, rc: top_rect(monitor, height), ..data(hwnd) };
    unsafe {
        SHAppBarMessage(ABM_QUERYPOS, &mut d);
        d.rc.bottom = d.rc.top + height;
        SHAppBarMessage(ABM_SETPOS, &mut d);
    }
    d.rc
}

pub fn remove(hwnd: HWND) {
    let mut d = data(hwnd);
    unsafe { SHAppBarMessage(ABM_REMOVE, &mut d) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_rect_spans_monitor_width() {
        let primary = RECT { left: 0, top: 0, right: 2560, bottom: 1440 };
        assert_eq!(top_rect(primary, 42), RECT { left: 0, top: 0, right: 2560, bottom: 42 });
        // Secondary monitor left of and above the primary one.
        let second = RECT { left: -1920, top: -200, right: 0, bottom: 880 };
        assert_eq!(top_rect(second, 28), RECT { left: -1920, top: -200, right: 0, bottom: -172 });
    }
}
