//! Sizes and colours. Fixed values until the settings file (F5.4).

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::{PCWSTR, w};

pub const MENUBAR_HEIGHT: f32 = 28.0;
pub const MENUBAR_PAD_X: f32 = 14.0;
/// Between status icons.
pub const MENUBAR_GAP: f32 = 16.0;
/// Between the last status icon and the clock, so the clock reads as its own group.
pub const CLOCK_GAP: f32 = 20.0;
/// Between a battery percentage and its glyph.
pub const PERCENT_GAP: f32 = 5.0;
/// Windows logo: two rows of two squares.
pub const LOGO_SQUARE: f32 = 6.0;
pub const LOGO_GAP: f32 = 1.0;
pub const LOGO_TO_NAME: f32 = 18.0;
/// Status icons give way before the app name gets narrower than this.
pub const NAME_MIN_WIDTH: f32 = 120.0;
/// Dock geometry that does not scale with the icon size (DIPs); see dock::Geometry.
/// Space between the dock and the screen edge.
pub const DOCK_MARGIN: f32 = 6.0;
pub const DOCK_DOT: f32 = 4.0;
pub const TEXT_FONT: &str = "Segoe UI Variable Text";
pub const TEXT_SIZE: f32 = 13.0;
pub const ICON_FONT: &str = "Segoe Fluent Icons";
pub const ICON_SIZE: f32 = 16.0;
pub const ICON_SIZE_LARGE: f32 = 20.0;
/// Glyphs that fill their whole em box (the globe).
pub const ICON_SIZE_SMALL: f32 = 13.5;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F {
            r: self.0 as f32 / 255.0,
            g: self.1 as f32 / 255.0,
            b: self.2 as f32 / 255.0,
            a: self.3 as f32 / 255.0,
        }
    }

    pub fn ui(self) -> windows::UI::Color {
        windows::UI::Color {
            R: self.0,
            G: self.1,
            B: self.2,
            A: self.3,
        }
    }

    pub fn opaque(self) -> Self {
        Rgba(self.0, self.1, self.2, 255)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// Bar fill laid over the blurred backdrop.
    pub bar: Rgba,
    pub text: Rgba,
    /// Empty Wi-Fi arcs under the signal glyph; menu separators.
    pub faint: Rgba,
    /// Menus and previews: tint over the blur, a little denser than the bar.
    pub popup: Rgba,
    /// Hovered preview card.
    pub hover: Rgba,
    /// Low battery. Fluent "critical" colours for each theme.
    pub alert: Rgba,
}

pub const DARK: Palette = Palette {
    bar: Rgba(28, 28, 30, 150),
    text: Rgba(255, 255, 255, 235),
    faint: Rgba(255, 255, 255, 78),
    popup: Rgba(40, 40, 42, 205),
    hover: Rgba(255, 255, 255, 28),
    alert: Rgba(255, 153, 164, 255),
};
pub const LIGHT: Palette = Palette {
    bar: Rgba(246, 246, 248, 165),
    text: Rgba(0, 0, 0, 220),
    faint: Rgba(0, 0, 0, 73),
    popup: Rgba(246, 246, 248, 215),
    hover: Rgba(0, 0, 0, 18),
    alert: Rgba(196, 43, 28, 255),
};

fn personalize_flag(name: PCWSTR) -> Option<bool> {
    let mut value = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
        .is_ok()
        .then_some(value != 0)
    }
}

/// System surfaces (taskbar, Start) follow `SystemUsesLightTheme`, not the app setting.
pub fn system_palette() -> Palette {
    if personalize_flag(w!("SystemUsesLightTheme")) == Some(true) {
        LIGHT
    } else {
        DARK
    }
}

/// App windows (our settings window) follow `AppsUseLightTheme`.
pub fn apps_light() -> bool {
    personalize_flag(w!("AppsUseLightTheme")) != Some(false)
}

/// The user's Windows accent colour (macOS uses its accent the same way).
pub fn accent() -> Rgba {
    let mut abgr = 0u32;
    let mut size = 4u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut abgr as *mut u32 as *mut _),
            Some(&mut size),
        )
        .is_ok()
    };
    if ok {
        Rgba(abgr as u8, (abgr >> 8) as u8, (abgr >> 16) as u8, 255)
    } else {
        Rgba(0, 120, 212, 255)
    }
}

/// Settings > Personalisation > Colours > Transparency effects. Missing value means on.
pub fn transparency() -> bool {
    personalize_flag(w!("EnableTransparency")) != Some(false)
}

pub fn scale(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

/// DIP length to whole device pixels.
pub fn px(dip: f32, dpi: u32) -> i32 {
    (dip * scale(dpi)).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_rounds_at_common_scales() {
        assert_eq!(px(MENUBAR_HEIGHT, 96), 28);
        assert_eq!(px(MENUBAR_HEIGHT, 120), 35);
        assert_eq!(px(MENUBAR_HEIGHT, 144), 42);
        assert_eq!(px(MENUBAR_HEIGHT, 168), 49);
    }
}

/// Windows-wide dark mode (system and apps), as the macOS Control Center toggle.
pub fn system_dark() -> bool {
    personalize_flag(w!("SystemUsesLightTheme")) == Some(false)
}

pub fn set_dark(dark: bool) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Registry::{REG_DWORD, RegSetKeyValueW};
    use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    let light: u32 = if dark { 0 } else { 1 };
    let key = w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    unsafe {
        for name in [w!("SystemUsesLightTheme"), w!("AppsUseLightTheme")] {
            _ = RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_DWORD.0, Some(&light as *const u32 as *const _), 4);
        }
        // Apps (and our own bars) repaint on this broadcast.
        _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("ImmersiveColorSet").0 as isize),
            SMTO_ABORTIFHUNG,
            200,
            None,
        );
    }
}

/// Windows "Animation effects" (Accessibility > Visual effects). Off means
/// opening, sliding and bouncing animations are skipped.
pub fn animations() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW};
    let mut on = windows::core::BOOL(1);
    unsafe {
        _ = SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    on.as_bool()
}

#[cfg(test)]
mod animation_tests {
    /// Real setting: `cargo test animation_setting -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn animation_setting() {
        println!("animations: {}", super::animations());
    }
}
