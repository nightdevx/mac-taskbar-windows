//! User settings in %LOCALAPPDATA%\taskbar\settings.txt, one `key = value` per
//! line. Unknown keys and bad values are ignored, so an old or hand-edited file
//! never stops the app. One copy lives on the UI thread; `update` saves it and
//! tells every listener (dock, menu bar) what changed.

use std::cell::RefCell;
use std::path::PathBuf;

pub const DOCK_ICON_MIN: f32 = 32.0;
pub const DOCK_ICON_MAX: f32 = 64.0;
/// Magnification is a scale: 1.0 is off.
pub const MAGNIFY_MAX: f32 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub dock_icon: f32,
    pub dock_autohide: bool,
    /// Dot under running apps.
    pub dock_indicators: bool,
    /// Largest icon scale under the pointer; 1.0 turns magnification off.
    pub dock_magnify: f32,
    /// Pinned apps as icon/launch sources (.lnk, exe path, `shell:AppsFolder\<AUMID>`).
    /// `None` until the user changes pins: then the Windows taskbar's pins are used.
    pub pins: Option<Vec<String>>,
    /// Folders right of the dock separator (stacks). `None`: Downloads.
    pub folders: Option<Vec<String>>,
    /// Apps hidden from Launchpad (Apps folder IDs).
    pub hidden_apps: Vec<String>,
    pub battery_percent: bool,
    /// Start at sign-in (HKCU Run entry).
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dock_icon: 48.0,
            dock_autohide: false,
            dock_indicators: true,
            dock_magnify: 1.5,
            pins: None,
            folders: None,
            hidden_apps: Vec::new(),
            battery_percent: true,
            autostart: false,
        }
    }
}

fn file() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("taskbar")
            .join("settings.txt"),
    )
}

pub fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    let flag = |v: &str, default: bool| match v {
        "true" => true,
        "false" => false,
        _ => default,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "dock_icon" => {
                if let Ok(v) = value.parse::<f32>() {
                    s.dock_icon = v.round().clamp(DOCK_ICON_MIN, DOCK_ICON_MAX);
                }
            }
            "dock_autohide" => s.dock_autohide = flag(value, s.dock_autohide),
            "dock_magnify" => {
                if let Ok(v) = value.parse::<f32>() {
                    s.dock_magnify = ((v * 20.0).round() / 20.0).clamp(1.0, MAGNIFY_MAX);
                }
            }
            "dock_indicators" => s.dock_indicators = flag(value, s.dock_indicators),
            "battery_percent" => s.battery_percent = flag(value, s.battery_percent),
            "autostart" => s.autostart = flag(value, s.autostart),
            "pins" if value.is_empty() => s.pins = Some(Vec::new()),
            "pin" => s.pins.get_or_insert_with(Vec::new).push(value.to_string()),
            "folders" if value.is_empty() => s.folders = Some(Vec::new()),
            "folder" => s.folders.get_or_insert_with(Vec::new).push(value.to_string()),
            "hide" => s.hidden_apps.push(value.to_string()),
            _ => {}
        }
    }
    s
}

pub fn format(s: &Settings) -> String {
    let mut out = format!(
        "dock_icon = {}\ndock_autohide = {}\ndock_indicators = {}\ndock_magnify = {}\nbattery_percent = {}\nautostart = {}\n",
        s.dock_icon, s.dock_autohide, s.dock_indicators, s.dock_magnify, s.battery_percent, s.autostart
    );
    match &s.pins {
        Some(pins) if pins.is_empty() => out.push_str("pins =\n"),
        Some(pins) => pins
            .iter()
            .for_each(|p| out.push_str(&format!("pin = {p}\n"))),
        None => {}
    }
    match &s.folders {
        Some(f) if f.is_empty() => out.push_str("folders =\n"),
        Some(f) => f.iter().for_each(|p| out.push_str(&format!("folder = {p}\n"))),
        None => {}
    }
    s.hidden_apps.iter().for_each(|id| out.push_str(&format!("hide = {id}\n")));
    out
}

/// Adds or removes our HKCU Run entry (no admin rights needed).
pub fn set_autostart(on: bool) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    use windows::core::w;
    let key = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = w!("nightdijital taskbar");
    let r = unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
            RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_SZ.0, Some(value.as_ptr() as *const _), (value.len() * 2) as u32)
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key, name)
        }
    };
    if r.is_err() && on {
        crate::log::error(&format!("autostart: {r:?}"));
    }
}

/// Pinned folders, Downloads until the user changes them.
pub fn folders(s: &Settings) -> Vec<String> {
    s.folders.clone().unwrap_or_else(|| downloads().into_iter().collect())
}

pub fn downloads() -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn load() -> Settings {
    file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| parse(&t))
        .unwrap_or_default()
}

fn save(s: &Settings) {
    let Some(f) = file() else { return };
    if let Some(dir) = f.parent() {
        _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&f, format(s)) {
        crate::log::error(&format!("settings save: {e}"));
    }
}

type Listener = Box<dyn Fn(&Settings, &Settings)>;

thread_local! {
    static CURRENT: RefCell<Settings> = RefCell::new(load());
    static LISTENERS: RefCell<Vec<Listener>> = const { RefCell::new(Vec::new()) };
}

pub fn get() -> Settings {
    CURRENT.with_borrow(Settings::clone)
}

/// Called with (before, after) on every change.
pub fn listen(f: impl Fn(&Settings, &Settings) + 'static) {
    LISTENERS.with_borrow_mut(|l| l.push(Box::new(f)));
}

/// Changes, saves and announces the settings. No-op when nothing changed.
pub fn update(change: impl FnOnce(&mut Settings)) {
    let before = get();
    let mut after = before.clone();
    change(&mut after);
    if after == before {
        return;
    }
    save(&after);
    let (b, a) = (format(&before), format(&after));
    let changed: Vec<&str> = a.lines().filter(|l| !b.lines().any(|x| x == *l)).collect();
    crate::log::info(&format!("settings: {}", changed.join(", ")));
    CURRENT.set(after.clone());
    // Listeners may call `get()`; the borrow above is already released.
    LISTENERS.with_borrow(|l| l.iter().for_each(|f| f(&before, &after)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes and removes the real HKCU Run entry: `cargo test autostart -- --ignored`.
    #[test]
    #[ignore]
    fn autostart_entry() {
        let read = || {
            std::process::Command::new("reg")
                .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "nightdijital taskbar"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default()
        };
        set_autostart(true);
        assert!(read().contains(".exe"), "{}", read());
        set_autostart(false);
        assert!(!read().contains(".exe"));
    }

    #[test]
    fn round_trip() {
        let s = Settings {
            dock_icon: 56.0,
            dock_autohide: true,
            dock_indicators: false,
            dock_magnify: 1.75,
            pins: Some(vec![r"C:\a\b.lnk".into(), r"shell:AppsFolder\X!App".into()]),
            folders: Some(vec![r"C:\Users\u\Downloads".into()]),
            hidden_apps: vec!["Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into()],
            battery_percent: false,
            autostart: true,
        };
        assert_eq!(parse(&format(&s)), s);
        let empty = Settings {
            pins: Some(Vec::new()),
            folders: Some(Vec::new()),
            ..Settings::default()
        };
        assert_eq!(
            parse(&format(&empty)),
            empty,
            "an emptied pin list stays empty"
        );
        assert_eq!(parse(&format(&Settings::default())), Settings::default());
    }

    #[test]
    fn bad_input_falls_back() {
        let s = parse(
            "dock_icon = huge\ndock_autohide = yes\nnonsense\n  dock_icon=500 \nbattery_percent = maybe\n",
        );
        assert_eq!(s.dock_icon, 64.0, "clamped, last valid line wins");
        assert!(!s.dock_autohide);
        assert!(s.battery_percent, "bad value keeps the default");
        assert_eq!(parse("dock_icon = 41.6").dock_icon, 42.0, "whole DIPs");
        assert_eq!(parse("dock_magnify = 9").dock_magnify, MAGNIFY_MAX);
        assert_eq!(
            parse("dock_magnify = 0.2").dock_magnify,
            1.0,
            "below 1.0 means off"
        );
        assert_eq!(parse(""), Settings::default());
    }

    #[test]
    fn update_notifies_only_on_change() {
        use std::rc::Rc;
        let calls = Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        listen(move |before, after| {
            assert_ne!(before, after);
            seen.set(seen.get() + 1);
        });
        CURRENT.set(Settings::default()); // isolate from the developer's real file
        let before = get();
        // Same value: no save, no call.
        update(|s| s.dock_icon = before.dock_icon);
        assert_eq!(calls.get(), 0);
    }
}
