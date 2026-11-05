//! Settings window, styled after macOS "Desktop & Dock": traffic lights,
//! grouped rows, a size slider and switches. Changes apply at once. The window
//! is destroyed on close so it costs nothing while unused.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{CompositionDrawingSurface, SpriteVisual};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::gfx::{self, Gfx, Label};
use crate::settings::{self, DOCK_ICON_MAX, DOCK_ICON_MIN, Settings};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};
use crate::log;

pub const CLASS: PCWSTR = w!("TaskbarSettings");
pub const TITLE: &str = "Masaüstü ve Dock";

// ---- layout (DIPs) ----

pub const WIDTH: f32 = 500.0;
const TITLE_H: f32 = 52.0;
const PAD: f32 = 20.0;
const HEADER_H: f32 = 30.0;
const ROW_H: f32 = 44.0;
const GROUP_GAP: f32 = 16.0;
const INNER: f32 = 14.0;
const RADIUS: f32 = 10.0;
const SWITCH_W: f32 = 34.0;
const SWITCH_H: f32 = 20.0;
const TRACK_W: f32 = 150.0;
const SLIDER_LABEL_W: f32 = 44.0;
const KNOB: f32 = 18.0;
const LIGHT_R: f32 = 6.0;
const LIGHT_Y: f32 = 26.0;
const LIGHT_X: [f32; 3] = [20.0, 40.0, 60.0];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Control {
    Size,
    AutoHide,
    Indicators,
    BatteryPercent,
}

/// Keyboard focus order, also the order rows are laid out in.
pub const CONTROLS: [Control; 4] = [Control::Size, Control::AutoHide, Control::Indicators, Control::BatteryPercent];
const GROUPS: [&str; 2] = ["Dock", "Menü çubuğu"];

impl Control {
    fn label(self) -> &'static str {
        match self {
            Control::Size => "Boyut",
            Control::AutoHide => "Dock'u otomatik olarak gizle ve göster",
            Control::Indicators => "Açık uygulamalar için göstergeleri göster",
            Control::BatteryPercent => "Pil yüzdesini göster",
        }
    }
    fn group(self) -> usize {
        match self {
            Control::BatteryPercent => 1,
            _ => 0,
        }
    }
    fn is_on(self, s: &Settings) -> bool {
        match self {
            Control::Size => false,
            Control::AutoHide => s.dock_autohide,
            Control::Indicators => s.dock_indicators,
            Control::BatteryPercent => s.battery_percent,
        }
    }
    fn toggle(self, s: &mut Settings) {
        match self {
            Control::Size => {}
            Control::AutoHide => s.dock_autohide ^= true,
            Control::Indicators => s.dock_indicators ^= true,
            Control::BatteryPercent => s.battery_percent ^= true,
        }
    }
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub struct Layout {
    pub groups: Vec<(&'static str, D2D_RECT_F)>,
    pub rows: Vec<(Control, D2D_RECT_F)>,
    pub height: f32,
}

pub fn layout() -> Layout {
    let (mut groups, mut rows) = (Vec::new(), Vec::new());
    let mut y = TITLE_H;
    for (g, title) in GROUPS.iter().enumerate() {
        let top = y + HEADER_H;
        let mut bottom = top;
        for c in CONTROLS.iter().filter(|c| c.group() == g) {
            rows.push((*c, rect(PAD, bottom, WIDTH - PAD, bottom + ROW_H)));
            bottom += ROW_H;
        }
        groups.push((*title, rect(PAD, top, WIDTH - PAD, bottom)));
        y = bottom + GROUP_GAP;
    }
    Layout { groups, rows, height: y - GROUP_GAP + PAD }
}

fn switch_rect(row: &D2D_RECT_F) -> D2D_RECT_F {
    let mid = (row.top + row.bottom) / 2.0;
    rect(row.right - INNER - SWITCH_W, mid - SWITCH_H / 2.0, row.right - INNER, mid + SWITCH_H / 2.0)
}

fn track_rect(row: &D2D_RECT_F) -> D2D_RECT_F {
    let mid = (row.top + row.bottom) / 2.0;
    let right = row.right - INNER - SLIDER_LABEL_W - 8.0;
    rect(right - TRACK_W, mid - 2.0, right, mid + 2.0)
}

/// Icon size for a pointer at `x` on the track, in whole DIPs.
pub fn slider_value(track: &D2D_RECT_F, x: f32) -> f32 {
    let t = ((x - track.left) / (track.right - track.left)).clamp(0.0, 1.0);
    (DOCK_ICON_MIN + t * (DOCK_ICON_MAX - DOCK_ICON_MIN)).round()
}

fn slider_x(track: &D2D_RECT_F, value: f32) -> f32 {
    track.left + (value - DOCK_ICON_MIN) / (DOCK_ICON_MAX - DOCK_ICON_MIN) * (track.right - track.left)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    Close,
    Minimize,
    Control(Control),
    /// On the size slider: drag starts here.
    Slider,
}

pub fn hit(l: &Layout, x: f32, y: f32) -> Option<Target> {
    for (i, cx) in LIGHT_X.iter().enumerate().take(2) {
        if (x - cx).hypot(y - LIGHT_Y) <= LIGHT_R + 2.0 {
            return Some(if i == 0 { Target::Close } else { Target::Minimize });
        }
    }
    let (c, row) = l.rows.iter().find(|(_, r)| inside(r, x, y))?;
    if *c == Control::Size {
        let t = track_rect(row);
        let grab = KNOB / 2.0 + 2.0;
        return (x >= t.left - grab && x <= t.right + grab).then_some(Target::Slider);
    }
    Some(Target::Control(*c))
}

// ---- drawing ----

#[derive(Clone, Copy)]
pub struct Palette {
    bg: Rgba,
    group: Rgba,
    border: Rgba,
    text: Rgba,
    secondary: Rgba,
    separator: Rgba,
    track: Rgba,
    accent: Rgba,
    knob: Rgba,
    shadow: Rgba,
}

pub fn palette(dark: bool) -> Palette {
    let accent = theme::accent();
    if dark {
        Palette {
            bg: Rgba(30, 30, 32, 255),
            group: Rgba(40, 40, 43, 255),
            border: Rgba(255, 255, 255, 18),
            text: Rgba(255, 255, 255, 225),
            secondary: Rgba(255, 255, 255, 140),
            separator: Rgba(255, 255, 255, 20),
            track: Rgba(255, 255, 255, 46),
            accent,
            knob: Rgba(236, 236, 236, 255),
            shadow: Rgba(0, 0, 0, 90),
        }
    } else {
        Palette {
            bg: Rgba(236, 236, 238, 255),
            group: Rgba(255, 255, 255, 255),
            border: Rgba(0, 0, 0, 18),
            text: Rgba(0, 0, 0, 218),
            secondary: Rgba(0, 0, 0, 128),
            separator: Rgba(0, 0, 0, 20),
            track: Rgba(0, 0, 0, 30),
            accent,
            knob: Rgba(255, 255, 255, 255),
            shadow: Rgba(0, 0, 0, 55),
        }
    }
}

pub struct Fonts {
    text: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    small: IDWriteTextFormat,
    cap: f32,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory) -> Result<Self> {
        Ok(Fonts {
            text: gfx::text_format(dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            bold: gfx::text_format(dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
            small: gfx::text_format(dwrite, theme::TEXT_FONT, 11.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            cap: gfx::cap_height(dwrite, theme::TEXT_FONT, 13.0)?,
        })
    }
}

pub struct View {
    pub settings: Settings,
    pub focus: Option<Control>,
    /// Window is active: traffic lights in colour, else grey (macOS).
    pub active: bool,
    pub palette: Palette,
}

fn ellipse(x: f32, y: f32, r: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r }
}

fn rounded(r: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }
}

/// Draws the whole window in DIPs.
pub fn draw(dc: &ID2D1DeviceContext, dwrite: &IDWriteFactory, fonts: &Fonts, l: &Layout, v: &View) -> Result<()> {
    let p = v.palette;
    unsafe {
        let brush = |c: Rgba| dc.CreateSolidColorBrush(&c.d2d(), None);
        let (text, secondary, accent) = (brush(p.text)?, brush(p.secondary)?, brush(p.accent)?);
        dc.Clear(Some(&p.bg.d2d()));

        // Traffic lights: close, minimise, and zoom greyed out (fixed size window).
        let lights = if v.active {
            [Rgba(255, 95, 87, 255), Rgba(254, 188, 46, 255), p.track]
        } else {
            [p.track, p.track, p.track]
        };
        for (cx, c) in LIGHT_X.iter().zip(lights) {
            dc.FillEllipse(&ellipse(*cx, LIGHT_Y, LIGHT_R), &brush(c)?);
        }
        let title = Label::new(dwrite, TITLE, &fonts.bold)?;
        title.draw_text(dc, (WIDTH - title.width) / 2.0, LIGHT_Y * 2.0, fonts.cap, &text);

        let separator = brush(p.separator)?;
        for (name, g) in &l.groups {
            Label::new(dwrite, name, &fonts.bold)?.draw_text(dc, g.left + 2.0, (g.top - HEADER_H / 2.0) * 2.0 + 2.0, fonts.cap, &text);
            dc.FillRoundedRectangle(&rounded(*g, RADIUS), &brush(p.group)?);
            dc.DrawRoundedRectangle(&rounded(*g, RADIUS), &brush(p.border)?, 1.0, None);
        }
        for (i, (c, row)) in l.rows.iter().enumerate() {
            let mid2 = row.top + row.bottom; // band trick: draw_text centres in [0, band]
            let label = Label::new(dwrite, c.label(), &fonts.text)?;
            label.draw_text(dc, row.left + INNER, mid2, fonts.cap, &text);
            let next_in_group = l.rows.get(i + 1).is_some_and(|(n, _)| n.group() == c.group());
            if next_in_group {
                dc.FillRectangle(&rect(row.left + INNER, row.bottom - 0.5, row.right - INNER, row.bottom + 0.5), &separator);
            }
            let focused = v.focus == Some(*c);
            match c {
                Control::Size => {
                    let t = track_rect(row);
                    let small = Label::new(dwrite, "Küçük", &fonts.small)?;
                    small.draw_text(dc, t.left - 8.0 - small.width, mid2, fonts.cap * 11.0 / 13.0, &secondary);
                    let big = Label::new(dwrite, "Büyük", &fonts.small)?;
                    big.draw_text(dc, t.right + 8.0, mid2, fonts.cap * 11.0 / 13.0, &secondary);
                    let kx = slider_x(&t, v.settings.dock_icon);
                    dc.FillRoundedRectangle(&rounded(t, 2.0), &brush(p.track)?);
                    dc.FillRoundedRectangle(&rounded(rect(t.left, t.top, kx, t.bottom), 2.0), &accent);
                    let ky = (t.top + t.bottom) / 2.0;
                    dc.FillEllipse(&ellipse(kx, ky + 0.75, KNOB / 2.0 + 0.5), &brush(p.shadow)?);
                    dc.FillEllipse(&ellipse(kx, ky, KNOB / 2.0), &brush(p.knob)?);
                    if focused {
                        dc.DrawEllipse(&ellipse(kx, ky, KNOB / 2.0 + 3.0), &accent, 2.0, None);
                    }
                }
                _ => {
                    let s = switch_rect(row);
                    let on = c.is_on(&v.settings);
                    dc.FillRoundedRectangle(&rounded(s, SWITCH_H / 2.0), &brush(if on { p.accent } else { p.track })?);
                    let r = SWITCH_H / 2.0 - 2.0;
                    let kx = if on { s.right - 2.0 - r } else { s.left + 2.0 + r };
                    let ky = (s.top + s.bottom) / 2.0;
                    dc.FillEllipse(&ellipse(kx, ky + 0.5, r + 0.5), &brush(p.shadow)?);
                    dc.FillEllipse(&ellipse(kx, ky, r), &brush(p.knob)?);
                    if focused {
                        let ring = rect(s.left - 3.0, s.top - 3.0, s.right + 3.0, s.bottom + 3.0);
                        dc.DrawRoundedRectangle(&rounded(ring, SWITCH_H / 2.0 + 3.0), &accent, 2.0, None);
                    }
                }
            }
        }
    }
    Ok(())
}

// ---- window ----

struct Comp {
    _target: DesktopWindowTarget,
    _visual: SpriteVisual,
    surface: CompositionDrawingSurface,
}

pub struct Prefs {
    hwnd: Cell<HWND>,
    gfx: Rc<Gfx>,
    fonts: Fonts,
    dpi: Cell<u32>,
    comp: RefCell<Option<Comp>>,
    focus: Cell<Option<Control>>,
    pressed: Cell<Option<Target>>,
    dragging: Cell<bool>,
    active: Cell<bool>,
}

thread_local! {
    static PREFS: OnceCell<&'static Prefs> = const { OnceCell::new() };
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let prefs: &'static Prefs = Box::leak(Box::new(Prefs {
        hwnd: Cell::new(HWND::default()),
        fonts: Fonts::new(&gfx.dwrite)?,
        gfx,
        dpi: Cell::new(96),
        comp: RefCell::new(None),
        focus: Cell::new(None),
        pressed: Cell::new(None),
        dragging: Cell::new(false),
        active: Cell::new(true),
    }));
    PREFS.with(|p| _ = p.set(prefs));
    Ok(())
}

/// Opens the window, or brings it forward when it is already open.
pub fn open() {
    let Some(p) = PREFS.with(|p| p.get().copied()) else { return };
    let hwnd = p.hwnd.get();
    if !hwnd.is_invalid() && unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        unsafe {
            _ = ShowWindow(hwnd, SW_RESTORE);
            _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    if let Err(e) = p.create() {
        log::error(&format!("settings window: {e}"));
    }
}

/// Settings changed elsewhere (dock menu): show the new values.
pub fn refresh() {
    if let Some(p) = PREFS.with(|p| p.get().copied()) {
        p.redraw();
    }
}

impl Prefs {
    fn create(&'static self) -> Result<()> {
        let hwnd = window::create_with(CLASS, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX, self)?;
        self.hwnd.set(hwnd);
        unsafe {
            let round = DWMWCP_ROUND;
            _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const _, 4);
            // Keeps the DWM shadow with the caption removed.
            _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 });
        }
        let c = &self.gfx.compositor;
        let target = self.gfx.target(hwnd)?;
        let surface = self.gfx.surface(1, 1)?;
        let visual = c.CreateSpriteVisual()?;
        visual.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        visual.SetBrush(&c.CreateSurfaceBrushWithSurface(&surface)?)?;
        target.SetRoot(&visual)?;
        *self.comp.borrow_mut() = Some(Comp { _target: target, _visual: visual, surface });
        self.focus.set(None);
        self.place();
        unsafe {
            _ = ShowWindow(hwnd, SW_SHOW);
            _ = SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    fn scale(&self) -> f32 {
        theme::scale(self.dpi.get())
    }

    /// Centres the window on the primary monitor at its DPI.
    fn place(&self) {
        let (monitor, dpi) = window::primary_monitor();
        self.dpi.set(dpi);
        let (w, h) = (theme::px(WIDTH, dpi), theme::px(layout().height, dpi));
        let x = (monitor.left + monitor.right - w) / 2;
        let y = monitor.top + (monitor.bottom - monitor.top - h) / 3;
        unsafe { _ = SetWindowPos(self.hwnd.get(), None, x, y, w, h, SWP_NOZORDER | SWP_FRAMECHANGED) };
        self.apply_theme();
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = self.gfx.resize(&c.surface, w as u32, h as u32);
        }
        self.redraw();
    }

    fn dark() -> bool {
        !theme::apps_light()
    }

    fn apply_theme(&self) {
        let dark = windows::core::BOOL::from(Self::dark());
        unsafe { _ = DwmSetWindowAttribute(self.hwnd.get(), DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4) };
    }

    fn redraw(&self) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let view = View { settings: settings::get(), focus: self.focus.get(), active: self.active.get(), palette: palette(Self::dark()) };
        let l = layout();
        if let Err(e) = self.gfx.draw(&c.surface, self.scale(), |dc| draw(dc, &self.gfx.dwrite, &self.fonts, &l, &view)) {
            log::error(&format!("settings draw: {e}"));
        }
    }

    fn point(&self, lp: LPARAM) -> (f32, f32) {
        let s = self.scale();
        ((lp.0 & 0xFFFF) as i16 as f32 / s, ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / s)
    }

    fn drag_to(&self, x: f32) {
        let l = layout();
        let Some((_, row)) = l.rows.iter().find(|(c, _)| *c == Control::Size) else { return };
        let v = slider_value(&track_rect(row), x);
        settings::update(|s| s.dock_icon = v);
    }

    fn step_slider(&self, delta: f32) {
        settings::update(|s| s.dock_icon = (s.dock_icon + delta).clamp(DOCK_ICON_MIN, DOCK_ICON_MAX));
    }

    fn activate(&self, t: Target) {
        match t {
            Target::Close => unsafe { _ = PostMessageW(Some(self.hwnd.get()), WM_CLOSE, WPARAM(0), LPARAM(0)) },
            Target::Minimize => unsafe { _ = ShowWindow(self.hwnd.get(), SW_MINIMIZE) },
            Target::Control(c) => settings::update(|s| c.toggle(s)),
            Target::Slider => {}
        }
    }

    fn move_focus(&self, back: bool) {
        let n = CONTROLS.len();
        let next = match self.focus.get().and_then(|f| CONTROLS.iter().position(|c| *c == f)) {
            Some(i) if back => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
            None if back => n - 1,
            None => 0,
        };
        self.focus.set(Some(CONTROLS[next]));
        self.redraw();
    }

    fn key(&self, vk: u16) -> bool {
        let down = |k: u16| unsafe { GetKeyState(k as i32) } < 0;
        match vk {
            k if k == VK_TAB.0 => self.move_focus(down(VK_SHIFT.0)),
            k if k == VK_ESCAPE.0 || (k == b'W' as u16 && down(VK_CONTROL.0)) => self.activate(Target::Close),
            k if k == VK_SPACE.0 || k == VK_RETURN.0 => match self.focus.get() {
                Some(c) if c != Control::Size => self.activate(Target::Control(c)),
                _ => return false,
            },
            k if (k == VK_LEFT.0 || k == VK_DOWN.0) && self.focus.get() == Some(Control::Size) => self.step_slider(-4.0),
            k if (k == VK_RIGHT.0 || k == VK_UP.0) && self.focus.get() == Some(Control::Size) => self.step_slider(4.0),
            _ => return false,
        }
        true
    }
}

impl Handler for Prefs {
    fn handle(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            // Client area covers the whole window: we draw our own title bar.
            WM_NCCALCSIZE if wp.0 != 0 => return Some(LRESULT(0)),
            WM_NCHITTEST => {
                let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                unsafe { _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt) };
                let s = self.scale();
                let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
                let caption = y < TITLE_H && hit(&layout(), x, y).is_none();
                return Some(LRESULT(if caption { HTCAPTION } else { HTCLIENT } as isize));
            }
            WM_ACTIVATE => {
                self.active.set((wp.0 & 0xFFFF) as u32 != WA_INACTIVE);
                self.redraw();
            }
            WM_LBUTTONDOWN => {
                let (x, y) = self.point(lp);
                let t = hit(&layout(), x, y);
                self.focus.set(None);
                self.pressed.set(t);
                if t == Some(Target::Slider) {
                    self.dragging.set(true);
                    unsafe { SetCapture(hwnd) };
                    self.drag_to(x);
                }
                self.redraw();
            }
            WM_MOUSEMOVE if self.dragging.get() => self.drag_to(self.point(lp).0),
            WM_LBUTTONUP => {
                if self.dragging.replace(false) {
                    unsafe { _ = ReleaseCapture() };
                } else {
                    let (x, y) = self.point(lp);
                    let t = hit(&layout(), x, y);
                    if t.is_some() && t == self.pressed.get() {
                        self.activate(t.unwrap());
                    }
                }
                self.pressed.set(None);
            }
            WM_KEYDOWN if self.key(wp.0 as u16) => {}
            WM_SETTINGCHANGE => {
                if lp.0 != 0 && unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() == "ImmersiveColorSet" {
                    self.apply_theme();
                    self.redraw();
                }
                return None;
            }
            WM_DPICHANGED => self.place(),
            WM_CLOSE => unsafe { _ = DestroyWindow(hwnd) },
            WM_DESTROY => {
                // Free the surface; the window is rebuilt on the next open.
                *self.comp.borrow_mut() = None;
                self.hwnd.set(HWND::default());
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_stack_inside_their_groups() {
        let l = layout();
        assert_eq!(l.rows.len(), CONTROLS.len());
        for (c, r) in &l.rows {
            let g = &l.groups[c.group()].1;
            assert!(r.top >= g.top && r.bottom <= g.bottom, "{c:?}");
        }
        for pair in l.rows.windows(2) {
            assert!(pair[1].1.top >= pair[0].1.bottom);
        }
        assert!(l.height > l.rows.last().unwrap().1.bottom);
    }

    #[test]
    fn hits_lights_switches_and_slider() {
        let l = layout();
        assert_eq!(hit(&l, LIGHT_X[0], LIGHT_Y), Some(Target::Close));
        assert_eq!(hit(&l, LIGHT_X[1], LIGHT_Y), Some(Target::Minimize));
        assert_eq!(hit(&l, LIGHT_X[2], LIGHT_Y), None, "zoom is disabled");
        assert_eq!(hit(&l, WIDTH / 2.0, 10.0), None, "title bar drags the window");
        let (_, auto) = l.rows.iter().find(|(c, _)| *c == Control::AutoHide).unwrap();
        assert_eq!(hit(&l, auto.left + 30.0, auto.top + 10.0), Some(Target::Control(Control::AutoHide)));
        let (_, size) = l.rows.iter().find(|(c, _)| *c == Control::Size).unwrap();
        let t = track_rect(size);
        assert_eq!(hit(&l, (t.left + t.right) / 2.0, (t.top + t.bottom) / 2.0), Some(Target::Slider));
        assert_eq!(hit(&l, size.left + 10.0, size.top + 10.0), None, "the size label is not a control");
    }

    #[test]
    fn slider_maps_ends_and_middle() {
        let t = rect(100.0, 0.0, 200.0, 4.0);
        assert_eq!(slider_value(&t, 50.0), DOCK_ICON_MIN);
        assert_eq!(slider_value(&t, 250.0), DOCK_ICON_MAX);
        assert_eq!(slider_value(&t, 150.0), 48.0);
        assert_eq!(slider_x(&t, 48.0), 150.0);
    }
}
