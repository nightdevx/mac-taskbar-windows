//! Which top-level windows get a dock entry, by the same rules as the Windows
//! taskbar, and decoding of the shell hook messages that change that set.

use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::BOOL;

#[derive(Clone, Copy, Default, Debug)]
pub struct WinInfo {
    pub visible: bool,
    pub ex_style: u32,
    pub owned: bool,
    /// Hidden by DWM: suspended Store app frames, windows on other virtual desktops.
    pub cloaked: bool,
}

/// Taskbar rules: visible and not cloaked; `WS_EX_APPWINDOW` forces an entry;
/// otherwise tool windows, owned windows and no-activate windows get none.
pub fn is_taskbar_window(i: &WinInfo) -> bool {
    if !i.visible || i.cloaked {
        return false;
    }
    if i.ex_style & WS_EX_APPWINDOW.0 != 0 {
        return true;
    }
    !i.owned && i.ex_style & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) == 0
}

pub fn info(hwnd: HWND) -> WinInfo {
    unsafe {
        let mut cloaked = 0u32;
        _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        WinInfo {
            visible: IsWindowVisible(hwnd).as_bool(),
            ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
            owned: GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()),
            cloaked: cloaked != 0,
        }
    }
}

pub fn qualifies(hwnd: HWND) -> bool {
    is_taskbar_window(&info(hwnd))
}

/// Taskbar windows that exist now, top of the z-order first.
pub fn enumerate() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
        if qualifies(hwnd) {
            unsafe { (*(lp.0 as *mut Vec<HWND>)).push(hwnd) };
        }
        TRUE
    }
    let mut out = Vec::new();
    unsafe { _ = EnumWindows(Some(collect), LPARAM(&mut out as *mut _ as isize)) };
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ShellEvent {
    Created(HWND),
    Destroyed(HWND),
    Activated(HWND),
    /// The window asks for attention (FlashWindow).
    Flash(HWND),
    /// Title or icon changed.
    Redraw(HWND),
    Replaced(HWND),
    Other,
}

const HIGHBIT: u32 = 0x8000;

/// Decodes a message registered as "SHELLHOOK" (wparam = code, lparam = window).
pub fn decode(wparam: usize, lparam: isize) -> ShellEvent {
    let hwnd = HWND(lparam as *mut _);
    let code = wparam as u32;
    match code & !HIGHBIT {
        HSHELL_WINDOWCREATED => ShellEvent::Created(hwnd),
        HSHELL_WINDOWDESTROYED => ShellEvent::Destroyed(hwnd),
        HSHELL_WINDOWACTIVATED => ShellEvent::Activated(hwnd),
        HSHELL_REDRAW if code & HIGHBIT != 0 => ShellEvent::Flash(hwnd),
        HSHELL_REDRAW => ShellEvent::Redraw(hwnd),
        HSHELL_WINDOWREPLACED => ShellEvent::Replaced(hwnd),
        _ => ShellEvent::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: WinInfo = WinInfo { visible: true, ex_style: 0, owned: false, cloaked: false };

    #[test]
    fn plain_visible_window_qualifies() {
        assert!(is_taskbar_window(&APP));
        assert!(!is_taskbar_window(&WinInfo { visible: false, ..APP }));
        assert!(!is_taskbar_window(&WinInfo { cloaked: true, ..APP }));
    }

    #[test]
    fn tool_owned_and_noactivate_windows_are_skipped() {
        assert!(!is_taskbar_window(&WinInfo { ex_style: WS_EX_TOOLWINDOW.0, ..APP }));
        assert!(!is_taskbar_window(&WinInfo { owned: true, ..APP }));
        assert!(!is_taskbar_window(&WinInfo { ex_style: WS_EX_NOACTIVATE.0, ..APP }));
    }

    #[test]
    fn appwindow_forces_an_entry_but_not_when_cloaked() {
        let forced = WinInfo { ex_style: WS_EX_APPWINDOW.0 | WS_EX_TOOLWINDOW.0, owned: true, ..APP };
        assert!(is_taskbar_window(&forced));
        assert!(!is_taskbar_window(&WinInfo { cloaked: true, ..forced }));
    }

    #[test]
    fn decodes_shell_codes() {
        let h = 0x1234isize;
        assert_eq!(decode(1, h), ShellEvent::Created(HWND(h as *mut _)));
        assert_eq!(decode(2, h), ShellEvent::Destroyed(HWND(h as *mut _)));
        assert_eq!(decode(0x8004, h), ShellEvent::Activated(HWND(h as *mut _)));
        assert_eq!(decode(0x8006, h), ShellEvent::Flash(HWND(h as *mut _)));
        assert_eq!(decode(6, h), ShellEvent::Redraw(HWND(h as *mut _)));
        assert_eq!(decode(13, h), ShellEvent::Replaced(HWND(h as *mut _)));
        assert_eq!(decode(99, h), ShellEvent::Other);
    }

    /// Manual check: `cargo test enumerate_matches_taskbar -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn enumerate_matches_taskbar() {
        for hwnd in enumerate() {
            let mut buf = [0u16; 128];
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
            println!("{:?} {}", hwnd.0, String::from_utf16_lossy(&buf[..n as usize]));
        }
    }
}
