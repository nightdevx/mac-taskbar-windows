//! User settings in %LOCALAPPDATA%\taskbar\settings.txt, one `key = value` per
//! line. Unknown keys and bad values are ignored, so an old or hand-edited file
//! never stops the app. One copy lives on the UI thread; `update` saves it and
//! tells every listener (dock, menu bar) what changed.

use std::cell::RefCell;
use std::path::PathBuf;

pub const DOCK_ICON_MIN: f32 = 32.0;
pub const DOCK_ICON_MAX: f32 = 64.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub dock_icon: f32,
    pub dock_autohide: bool,
    /// Dot under running apps.
    pub dock_indicators: bool,
    /// Pinned apps as icon/launch sources (.lnk, exe path, `shell:AppsFolder\<AUMID>`).
    /// `None` until the user changes pins: then the Windows taskbar's pins are used.
    pub pins: Option<Vec<String>>,
    pub battery_percent: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { dock_icon: 48.0, dock_autohide: false, dock_indicators: true, pins: None, battery_percent: true }
    }
}

fn file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("taskbar").join("settings.txt"))
}

pub fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    let flag = |v: &str, default: bool| match v {
        "true" => true,
        "false" => false,
        _ => default,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "dock_icon" => {
                if let Ok(v) = value.parse::<f32>() {
                    s.dock_icon = v.round().clamp(DOCK_ICON_MIN, DOCK_ICON_MAX);
                }
            }
            "dock_autohide" => s.dock_autohide = flag(value, s.dock_autohide),
            "dock_indicators" => s.dock_indicators = flag(value, s.dock_indicators),
            "battery_percent" => s.battery_percent = flag(value, s.battery_percent),
            "pins" if value.is_empty() => s.pins = Some(Vec::new()),
            "pin" => s.pins.get_or_insert_with(Vec::new).push(value.to_string()),
            _ => {}
        }
    }
    s
}

pub fn format(s: &Settings) -> String {
    let mut out = format!(
        "dock_icon = {}\ndock_autohide = {}\ndock_indicators = {}\nbattery_percent = {}\n",
        s.dock_icon, s.dock_autohide, s.dock_indicators, s.battery_percent
    );
    match &s.pins {
        Some(pins) if pins.is_empty() => out.push_str("pins =\n"),
        Some(pins) => pins.iter().for_each(|p| out.push_str(&format!("pin = {p}\n"))),
        None => {}
    }
    out
}

fn load() -> Settings {
    file().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| parse(&t)).unwrap_or_default()
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
    CURRENT.set(after.clone());
    // Listeners may call `get()`; the borrow above is already released.
    LISTENERS.with_borrow(|l| l.iter().for_each(|f| f(&before, &after)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Settings {
            dock_icon: 56.0,
            dock_autohide: true,
            dock_indicators: false,
            pins: Some(vec![r"C:\a\b.lnk".into(), r"shell:AppsFolder\X!App".into()]),
            battery_percent: false,
        };
        assert_eq!(parse(&format(&s)), s);
        let empty = Settings { pins: Some(Vec::new()), ..Settings::default() };
        assert_eq!(parse(&format(&empty)), empty, "an emptied pin list stays empty");
        assert_eq!(parse(&format(&Settings::default())), Settings::default());
    }

    #[test]
    fn bad_input_falls_back() {
        let s = parse("dock_icon = huge\ndock_autohide = yes\nnonsense\n  dock_icon=500 \nbattery_percent = maybe\n");
        assert_eq!(s.dock_icon, 64.0, "clamped, last valid line wins");
        assert!(!s.dock_autohide);
        assert!(s.battery_percent, "bad value keeps the default");
        assert_eq!(parse("dock_icon = 41.6").dock_icon, 42.0, "whole DIPs");
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
