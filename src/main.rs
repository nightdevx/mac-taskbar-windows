#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appbar;
mod appid;
mod brightness;
mod calendar;
mod clock;
mod dock;
mod gfx;
mod icons;
mod launchpad;
mod log;
mod magnify;
mod menu;
mod menubar;
mod panels;
mod popup;
mod power;
mod prefs;
mod preview;
mod settings;
mod shelltray;
mod snapshot;
mod stack;
mod status;
mod theme;
mod touchpad;
mod tray;
mod wifi;
mod window;
mod winlist;

use std::rc::Rc;
use std::time::Instant;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, MSG, PostMessageW, TranslateMessage, WM_CLOSE,
};
use windows::core::{BOOL, Result, w};

fn main() {
    log::install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let result = if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        snapshot::run(args.get(i + 1).map_or("snapshot.png", String::as_str))
    } else if flag("--restore") {
        shelltray::restore();
        Ok(())
    } else if flag("--quit") {
        quit_running();
        Ok(())
    } else {
        run(flag("--hidden"), flag("--settings"))
    };
    if let Err(e) = result {
        log::error(&format!("fatal: {e}"));
        std::process::exit(1);
    }
}

fn run(hidden: bool, open_settings: bool) -> Result<()> {
    let started = Instant::now();
    let _single = unsafe { CreateMutexW(None, true, w!(r"Local\nightdijital.taskbar"))? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if open_settings && let Ok(dock) = unsafe { FindWindowW(dock::CLASS, None) } {
            unsafe { _ = PostMessageW(Some(dock), dock::WM_OPEN_SETTINGS, WPARAM(0), LPARAM(0)) };
        }
        log::info("already running, exiting");
        return Ok(());
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        _ = SetConsoleCtrlHandler(Some(on_console_ctrl), true);
    }
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })?
    };

    let gfx = Rc::new(gfx::Gfx::new()?);
    let bar = menubar::MenuBar::create(gfx.clone(), hidden)?;
    let dock = dock::Dock::create(gfx.clone(), hidden)?;
    prefs::init(gfx.clone())?;
    menu::init(gfx.clone())?;
    panels::init(gfx.clone())?;
    stack::init(gfx.clone())?;
    launchpad::init(gfx.clone())?;
    preview::init(
        gfx,
        unsafe { FindWindowW(dock::CLASS, None) }.unwrap_or_default(),
    )?;
    settings::listen(move |before, after| {
        if before.autostart != after.autostart {
            settings::set_autostart(after.autostart);
        }
        dock.apply(before, after);
        bar.apply(before, after);
        prefs::refresh();
    });
    if open_settings {
        prefs::open();
    }
    if !hidden {
        shelltray::hide();
        unsafe { SetUnhandledExceptionFilter(Some(on_crash)) };
        tray::start(unsafe { FindWindowW(menubar::CLASS, None) }.unwrap_or_default());
    }
    log::info(&format!(
        "started in {} ms (hidden: {hidden})",
        started.elapsed().as_millis()
    ));

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if !hidden {
        shelltray::restore();
    }
    log::info("exit");
    Ok(())
}

/// `--quit`: asks the running instance to close cleanly (gives back the screen edge).
pub(crate) fn quit_running() {
    // Dock first: the menu bar's WM_DESTROY ends the message loop.
    for class in [dock::CLASS, menubar::CLASS] {
        unsafe {
            if let Ok(hwnd) = FindWindowW(class, None) {
                _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Last chance after a crash outside Rust's panic machinery: give the taskbar back.
unsafe extern "system" fn on_crash(_: *const EXCEPTION_POINTERS) -> i32 {
    log::error("unhandled exception");
    shelltray::restore();
    0 // EXCEPTION_CONTINUE_SEARCH
}

/// Ctrl+C in a debug console closes cleanly instead of leaving a reserved edge behind.
unsafe extern "system" fn on_console_ctrl(_: u32) -> BOOL {
    quit_running();
    true.into()
}
