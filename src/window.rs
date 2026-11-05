//! Shared plumbing for our shell windows (menu bar, dock): creation, message
//! dispatch to a `'static` handler, primary monitor geometry, blurred backdrop.

use windows::UI::Composition::{CompositionColorBrush, Compositor, SpriteVisual};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_HOSTBACKDROPBRUSH, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result};
use windows_numerics::Vector2;

use crate::theme;

pub trait Handler {
    /// `None` falls through to `DefWindowProcW`.
    fn handle(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT>;
}

unsafe extern "system" fn wndproc<T: Handler + 'static>(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        let handler = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const T;
        if let Some(h) = handler.as_ref()
            && let Some(r) = h.handle(hwnd, msg, wp, lp)
        {
            return r;
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// Topmost, never-activated popup without a taskbar entry, drawn only through
/// composition. Messages go to `handler`, which lives for the whole process.
pub fn create<T: Handler + 'static>(class: PCWSTR, handler: &'static T) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc::<T>),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            class,
            class,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some(handler as *const T as *const _),
        )
    }
}

/// Primary monitor rect and effective DPI.
pub fn primary_monitor() -> (RECT, u32) {
    unsafe {
        let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (mut dpi, mut dpi_y) = (96, 96);
        _ = GetMonitorInfoW(monitor, &mut mi);
        _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        (mi.rcMonitor, dpi)
    }
}

/// Blurred desktop plus a tint layer, both filling their parent.
pub struct Backdrop {
    pub blur: SpriteVisual,
    pub tint_visual: SpriteVisual,
    pub tint: CompositionColorBrush,
}

impl Backdrop {
    pub fn new(c: &Compositor, hwnd: HWND) -> Result<Self> {
        let full = Vector2 { X: 1.0, Y: 1.0 };
        unsafe {
            let on = windows::core::BOOL(1);
            DwmSetWindowAttribute(hwnd, DWMWA_USE_HOSTBACKDROPBRUSH, &on as *const _ as *const _, 4)?;
        }
        let blur = c.CreateSpriteVisual()?;
        blur.SetRelativeSizeAdjustment(full)?;
        blur.SetBrush(&c.CreateHostBackdropBrush()?)?;
        let tint = c.CreateColorBrush()?;
        let tint_visual = c.CreateSpriteVisual()?;
        tint_visual.SetRelativeSizeAdjustment(full)?;
        tint_visual.SetBrush(&tint)?;
        Ok(Self { blur, tint_visual, tint })
    }

    /// Tint over blur, or the same colour opaque when transparency effects are off.
    pub fn apply(&self, color: theme::Rgba) {
        let blur = theme::transparency();
        _ = self.blur.SetIsVisible(blur);
        _ = self.tint.SetColor(if blur { color } else { color.opaque() }.ui());
    }
}
