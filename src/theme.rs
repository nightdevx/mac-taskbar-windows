//! Sizes and colours. Fixed values until the settings file (F5.4).

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::w;

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
pub const TEXT_FONT: &str = "Segoe UI Variable Text";
pub const TEXT_SIZE: f32 = 13.0;
pub const ICON_FONT: &str = "Segoe Fluent Icons";
pub const ICON_SIZE: f32 = 16.0;
pub const ICON_SIZE_LARGE: f32 = 20.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.0 as f32 / 255.0, g: self.1 as f32 / 255.0, b: self.2 as f32 / 255.0, a: self.3 as f32 / 255.0 }
    }

    pub fn ui(self) -> windows::UI::Color {
        windows::UI::Color { R: self.0, G: self.1, B: self.2, A: self.3 }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// Bar fill laid over the blurred backdrop.
    pub bar: Rgba,
    pub text: Rgba,
    /// Low battery. Fluent "critical" colours for each theme.
    pub alert: Rgba,
}

pub const DARK: Palette = Palette { bar: Rgba(28, 28, 30, 150), text: Rgba(255, 255, 255, 235), alert: Rgba(255, 153, 164, 255) };
pub const LIGHT: Palette = Palette { bar: Rgba(246, 246, 248, 165), text: Rgba(0, 0, 0, 220), alert: Rgba(196, 43, 28, 255) };

/// System surfaces (taskbar, Start) follow `SystemUsesLightTheme`, not the app setting.
pub fn system_palette() -> Palette {
    let mut value = 0u32;
    let mut size = 4u32;
    let light = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
        .is_ok()
    } && value == 1;
    if light { LIGHT } else { DARK }
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
