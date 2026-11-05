//! Hides the Windows taskbar while we run and always gives it back: on exit,
//! session end, panic, unhandled exception, or `--restore` after a hard kill.
//!
//! A hidden taskbar still reserves its screen strip, so it is switched to
//! auto-hide first. The user's own auto-hide setting is saved to a file before
//! that, so even a crash cannot lose it.

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Shell::{
    ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::log;

fn state_file() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("taskbar")
            .join("autohide-before.txt"),
    )
}

/// Windows taskbar windows: the main one and one per extra monitor.
fn trays() -> Vec<HWND> {
    let mut out = Vec::new();
    for class in [w!("Shell_TrayWnd"), w!("Shell_SecondaryTrayWnd")] {
        let mut after = None;
        while let Ok(h) = unsafe { FindWindowExW(None, after, class, PCWSTR::null()) } {
            out.push(h);
            after = Some(h);
        }
    }
    out
}

fn tray_data(tray: HWND, state: u32) -> APPBARDATA {
    APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: tray,
        lParam: LPARAM(state as isize),
        ..Default::default()
    }
}

fn set_autohide(tray: HWND, on: bool, base: u32) {
    let state = if on {
        base | ABS_AUTOHIDE
    } else {
        base & !ABS_AUTOHIDE
    };
    let mut d = tray_data(tray, state);
    unsafe { SHAppBarMessage(ABM_SETSTATE, &mut d) };
}

/// Hides every Windows taskbar. Safe to call again after Explorer restarts.
pub fn hide() {
    let trays = trays();
    let Some(&main) = trays.first() else {
        return log::error("Windows taskbar not found");
    };
    let mut d = tray_data(main, 0);
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut d) } as u32;
    // Keep the first saved value: after a crash the current state is our own auto-hide.
    if let Some(f) = state_file()
        && !f.exists()
    {
        _ = std::fs::write(&f, state.to_string());
    }
    set_autohide(main, true, state);
    for t in &trays {
        unsafe { _ = ShowWindow(*t, SW_HIDE) };
    }
    watch(main);
}

/// Shows the Windows taskbar again with the user's saved auto-hide setting.
pub fn restore() {
    let trays = trays();
    let saved = state_file()
        .and_then(|f| std::fs::read_to_string(&f).ok())
        .and_then(|s| s.trim().parse::<u32>().ok());
    if let (Some(&main), Some(state)) = (trays.first(), saved) {
        set_autohide(main, state & ABS_AUTOHIDE != 0, state);
    }
    for t in &trays {
        unsafe { _ = ShowWindow(*t, SW_SHOWNA) };
    }
    if let Some(f) = state_file() {
        _ = std::fs::remove_file(f);
    }
}

/// Explorer shows its taskbar again on some events (Start, Win+T); hide it back.
/// The hook is limited to Explorer's process, so it does not wake us for other apps.
fn watch(tray: HWND) {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(tray, Some(&mut pid));
        SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            None,
            Some(on_show),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
    }
}

unsafe extern "system" fn on_show(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    id_object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if id_object == OBJID_WINDOW.0 && trays().contains(&hwnd) {
        unsafe { _ = ShowWindow(hwnd, SW_HIDE) };
    }
}
