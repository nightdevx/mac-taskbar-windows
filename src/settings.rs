//! User settings in %LOCALAPPDATA%\taskbar\settings.txt, one `key = value` per
//! line. Unknown keys and bad values are ignored, so an old or hand-edited file
//! never stops the app.

use std::path::PathBuf;

/// Dock icon sizes offered in the menu (DIPs), like the Ubuntu dock's slider stops.
pub const DOCK_SIZES: [f32; 5] = [32.0, 40.0, 48.0, 56.0, 64.0];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub dock_icon: f32,
    pub dock_autohide: bool,
    /// Pinned apps as icon/launch sources (.lnk, exe path, `shell:AppsFolder\<AUMID>`).
    /// `None` until the user changes pins: then the Windows taskbar's pins are used.
    pub pins: Option<Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { dock_icon: 48.0, dock_autohide: false, pins: None }
    }
}

fn file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("taskbar").join("settings.txt"))
}

pub fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "dock_icon" => {
                if let Ok(v) = value.parse::<f32>() {
                    s.dock_icon = v.clamp(DOCK_SIZES[0], DOCK_SIZES[DOCK_SIZES.len() - 1]);
                }
            }
            "dock_autohide" => s.dock_autohide = value == "true",
            "pins" if value.is_empty() => s.pins = Some(Vec::new()),
            "pin" => s.pins.get_or_insert_with(Vec::new).push(value.to_string()),
            _ => {}
        }
    }
    s
}

pub fn format(s: &Settings) -> String {
    let mut out = format!("dock_icon = {}\ndock_autohide = {}\n", s.dock_icon, s.dock_autohide);
    match &s.pins {
        Some(pins) if pins.is_empty() => out.push_str("pins =\n"),
        Some(pins) => pins.iter().for_each(|p| out.push_str(&format!("pin = {p}\n"))),
        None => {}
    }
    out
}

pub fn load() -> Settings {
    file().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| parse(&t)).unwrap_or_default()
}

pub fn save(s: &Settings) {
    let Some(f) = file() else { return };
    if let Some(dir) = f.parent() {
        _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&f, format(s)) {
        crate::log::error(&format!("settings save: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Settings { dock_icon: 56.0, dock_autohide: true, pins: Some(vec![r"C:\a\b.lnk".into(), r"shell:AppsFolder\X!App".into()]) };
        assert_eq!(parse(&format(&s)), s);
        let empty = Settings { pins: Some(Vec::new()), ..Settings::default() };
        assert_eq!(parse(&format(&empty)), empty, "an emptied pin list stays empty");
        assert_eq!(parse(&format(&Settings::default())), Settings::default());
    }

    #[test]
    fn bad_input_falls_back() {
        let s = parse("dock_icon = huge\ndock_autohide = yes\nnonsense\n  dock_icon=500 \n");
        assert_eq!(s.dock_icon, 64.0, "clamped, last valid line wins");
        assert!(!s.dock_autohide);
        assert_eq!(parse(""), Settings::default());
    }
}
