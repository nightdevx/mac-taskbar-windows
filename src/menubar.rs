//! Top menu bar: system symbol, active app name, status icons, clock.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{CompositionDrawingSurface, SpriteVisual};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Shell::{ABN_FULLSCREENAPP, ABN_POSCHANGED};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::gfx::{self, Gfx, Label};
use crate::status::{self, Icon, Sources};
use crate::theme::{self, Palette};
use crate::window::{self, Backdrop, Handler};
use crate::{appbar, appid, clock, log, menu, panels, popup};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

pub const CLASS: PCWSTR = w!("TaskbarMenuBar");
/// Bars on the other monitors: own class, so `--quit` finds the main bar.
const CLASS_OTHER: PCWSTR = w!("TaskbarMenuBarOther");
const WM_APPBAR: u32 = WM_APP + 1;
const WM_FOREGROUND: u32 = WM_APP + 2;
/// Tray icons changed (`tray.rs`).
pub const WM_TRAY: u32 = WM_APP + 20;
const TIMER_CLOCK: usize = 1;

thread_local! {
    /// Target for hook callbacks, which get no user data.
    static BAR_HWND: Cell<HWND> = Cell::new(HWND::default());
    /// Bars on the other monitors, mirroring the main bar (macOS shows one per display).
    static OTHERS: RefCell<Vec<&'static MenuBar>> = const { RefCell::new(Vec::new()) };
}

pub struct Fonts {
    pub text: IDWriteTextFormat,
    pub bold: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
    pub icons_small: IDWriteTextFormat,
    pub icons_large: IDWriteTextFormat,
    /// Cap height of the text font, for optical vertical centring.
    pub cap: f32,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory) -> Result<Self> {
        Ok(Self {
            text: gfx::text_format(
                dwrite,
                theme::TEXT_FONT,
                theme::TEXT_SIZE,
                DWRITE_FONT_WEIGHT_REGULAR,
            )?,
            bold: gfx::text_format(
                dwrite,
                theme::TEXT_FONT,
                theme::TEXT_SIZE,
                DWRITE_FONT_WEIGHT_BOLD,
            )?,
            icons: gfx::text_format(
                dwrite,
                theme::ICON_FONT,
                theme::ICON_SIZE,
                DWRITE_FONT_WEIGHT_REGULAR,
            )?,
            icons_small: gfx::text_format(
                dwrite,
                theme::ICON_FONT,
                theme::ICON_SIZE_SMALL,
                DWRITE_FONT_WEIGHT_REGULAR,
            )?,
            icons_large: gfx::text_format(
                dwrite,
                theme::ICON_FONT,
                theme::ICON_SIZE_LARGE,
                DWRITE_FONT_WEIGHT_REGULAR,
            )?,
            cap: gfx::cap_height(dwrite, theme::TEXT_FONT, theme::TEXT_SIZE)?,
        })
    }
}

#[derive(Clone)]
pub struct State {
    pub app_name: String,
    pub clock: String,
    /// Status icons, drawn right to left before the clock.
    pub status: Vec<Icon>,
    /// Notification area icons, left of the status icons (macOS menu extras).
    pub tray: Vec<crate::tray::Entry>,
    pub palette: Palette,
}

/// Windows logo as the system symbol (macOS puts its logo here): four squares
/// snapped to device pixels so the gaps stay crisp at every scale. Returns its width.
fn draw_logo(dc: &ID2D1DeviceContext, x: f32, h: f32, brush: &ID2D1SolidColorBrush) -> f32 {
    let snap = gfx::pixel_snapper(dc);
    let one_px = snap(1.0).max(snap(0.75)); // at least one device pixel
    let (side, gap) = (snap(theme::LOGO_SQUARE), snap(theme::LOGO_GAP).max(one_px));
    let size = 2.0 * side + gap;
    let (x0, y0) = (snap(x), snap((h - size) / 2.0));
    unsafe {
        dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        for (cx, cy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let (l, t) = (x0 + cx * (side + gap), y0 + cy * (side + gap));
            dc.FillRectangle(
                &D2D_RECT_F {
                    left: l,
                    top: t,
                    right: l + side,
                    bottom: t + side,
                },
                brush,
            );
        }
        dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
    }
    size
}

/// Draws bar content (not the background) in DIPs on a transparent target.
/// Right side first: clock and status icons keep their place, the app name gets the rest.
/// Clickable parts of the bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Logo,
    Clock,
    Status(status::Kind),
    /// Index into `State::tray`.
    Tray(usize),
}

/// Horizontal extent of a clickable part (DIPs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zone {
    pub hit: Hit,
    pub left: f32,
    pub right: f32,
}

/// Draws the bar and returns where each clickable part ended up.
pub fn draw(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    fonts: &Fonts,
    st: &State,
    w: f32,
    h: f32,
) -> Result<Vec<Zone>> {
    let mut zones = Vec::new();
    let brush = unsafe { dc.CreateSolidColorBrush(&st.palette.text.d2d(), None)? };
    let faint = unsafe { dc.CreateSolidColorBrush(&st.palette.faint.d2d(), None)? };
    let alert = unsafe { dc.CreateSolidColorBrush(&st.palette.alert.d2d(), None)? };
    let pad = theme::MENUBAR_PAD_X;

    let logo_w = draw_logo(dc, pad, h, &brush);
    zones.push(Zone { hit: Hit::Logo, left: 0.0, right: pad + logo_w + theme::LOGO_TO_NAME / 2.0 });
    let left = pad + logo_w + theme::LOGO_TO_NAME;
    let name_floor = left + theme::NAME_MIN_WIDTH;

    // Items are placed right to left; `edge` is the left ink edge of the last one,
    // so every gap is measured ink to ink whatever the glyph's side bearings.
    let clock = Label::new(dwrite, &st.clock, &fonts.text)?;
    let clock_x = w - pad - clock.width;
    clock.draw_text(dc, clock_x, h, fonts.cap, &brush);
    zones.push(Zone { hit: Hit::Clock, left: clock_x - theme::CLOCK_GAP / 2.0, right: w });
    let mut edge = clock_x + clock.ink_left;
    let mut gap = theme::CLOCK_GAP;

    for icon in &st.status {
        let font = match icon.size {
            status::IconSize::Small => &fonts.icons_small,
            status::IconSize::Normal => &fonts.icons,
            status::IconSize::Large => &fonts.icons_large,
        };
        let glyph = Label::new(dwrite, &icon.glyph.to_string(), font)?;
        let gx = edge - gap - glyph.width + glyph.ink_right;
        let text = icon
            .percent
            .map(|p| Label::new(dwrite, &clock::percent(p), &fonts.text))
            .transpose()?;
        let tx = text
            .as_ref()
            .map(|t| gx + glyph.ink_left - theme::PERCENT_GAP - t.width + t.ink_right);
        let new_edge = match (&text, tx) {
            (Some(t), Some(tx)) => tx + t.ink_left,
            _ => gx + glyph.ink_left,
        };
        if new_edge - theme::MENUBAR_GAP < name_floor {
            break; // ponytail: icons that do not fit are dropped; overflow menu when the tray lands (F5.1)
        }
        if let (Some(t), Some(tx)) = (&text, tx) {
            t.draw_text(dc, tx, h, fonts.cap, &brush);
        }
        if let Some(under) = icon.under {
            Label::new(dwrite, &under.to_string(), font)?.draw(dc, gx, h, &faint);
        }
        glyph.draw(dc, gx, h, if icon.alert { &alert } else { &brush });
        zones.push(Zone { hit: Hit::Status(icon.kind), left: new_edge - gap / 2.0, right: edge - gap / 2.0 });
        edge = new_edge;
        gap = theme::MENUBAR_GAP;
    }

    let px_props = windows::Win32::Graphics::Direct2D::D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: windows::Win32::Graphics::Direct2D::Common::D2D1_PIXEL_FORMAT {
            format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: windows::Win32::Graphics::Direct2D::Common::D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
    };
    let size = crate::tray::ICON_DIP;
    for (i, icon) in st.tray.iter().enumerate() {
        let side = ((icon.pixels.len() / 4) as f64).sqrt() as u32;
        let x = edge - gap - size;
        if x - theme::MENUBAR_GAP < name_floor || side == 0 {
            break; // ponytail: icons that do not fit are dropped; an overflow menu if people miss them
        }
        let y = ((h - size) / 2.0).round();
        unsafe {
            let bmp = dc.CreateBitmap(
                windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U { width: side, height: side },
                Some(icon.pixels.as_ptr() as *const _),
                side * 4,
                &px_props,
            )?;
            let r = D2D_RECT_F { left: x, top: y, right: x + size, bottom: y + size };
            dc.DrawBitmap(&bmp, Some(&r), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
        }
        zones.push(Zone { hit: Hit::Tray(i), left: x - gap / 2.0, right: edge - gap / 2.0 });
        edge = x;
        gap = theme::MENUBAR_GAP;
    }

    if !st.app_name.is_empty() {
        let max = edge - theme::MENUBAR_GAP - left;
        Label::fitted(dwrite, &st.app_name, &fonts.bold, max)?
            .draw_text(dc, left, h, fonts.cap, &brush);
    }
    Ok(zones)
}

struct Comp {
    _target: DesktopWindowTarget,
    backdrop: Backdrop,
    content: SpriteVisual,
    surface: CompositionDrawingSurface,
}

pub struct MenuBar {
    hwnd: Cell<HWND>,
    gfx: Rc<Gfx>,
    fonts: Fonts,
    /// `--hidden`: no window shown, no screen edge reserved (resource measurements).
    hidden: bool,
    dpi: Cell<u32>,
    size: Cell<(i32, i32)>,
    state: RefCell<State>,
    comp: RefCell<Option<Comp>>,
    zones: RefCell<Vec<Zone>>,
    sources: OnceCell<Sources>,
    status: Cell<status::Status>,
    /// Guards `place()`: the AppBar calls dispatch sent messages and can re-enter.
    placing: Cell<bool>,
    taskbar_created: u32,
    /// Another monitor's bar: its screen; `None` for the main bar (primary monitor).
    area: Cell<Option<RECT>>,
    /// Registered as an AppBar (other bars drop it when their monitor goes away).
    registered: Cell<bool>,
}

impl MenuBar {
    pub fn create(gfx: Rc<Gfx>, hidden: bool) -> Result<&'static MenuBar> {
        let palette = theme::system_palette();
        let bar: &'static MenuBar = Box::leak(Box::new(MenuBar {
            hwnd: Cell::new(HWND::default()),
            fonts: Fonts::new(&gfx.dwrite)?,
            gfx,
            hidden,
            dpi: Cell::new(0),
            size: Cell::new((0, 0)),
            state: RefCell::new(State {
                app_name: String::new(),
                clock: String::new(),
                status: Vec::new(),
                tray: Vec::new(),
                palette,
            }),
            comp: RefCell::new(None),
            zones: RefCell::new(Vec::new()),
            sources: OnceCell::new(),
            status: Cell::new(status::Status::default()),
            placing: Cell::new(false),
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            area: Cell::new(None),
            registered: Cell::new(!hidden),
        }));

        let hwnd = window::create(CLASS, bar)?;
        bar.hwnd.set(hwnd);
        BAR_HWND.set(hwnd);
        bar.init_visuals()?;
        if !hidden {
            appbar::register(hwnd, WM_APPBAR);
        }
        bar.place();
        bar.sync_monitors();
        unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(on_foreground),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
        }
        bar.set_app(unsafe { GetForegroundWindow() });
        let sources = Sources::start(hwnd);
        let st = sources.read();
        log::info(&format!("status at start: {st:?}"));
        _ = bar.sources.set(sources);
        bar.set_status(st);
        bar.refresh_clock();
        if !hidden {
            unsafe { _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        }
        Ok(bar)
    }

    fn init_visuals(&self) -> Result<()> {
        let c = &self.gfx.compositor;
        let target = self.gfx.target(self.hwnd.get())?;
        let root = c.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        let backdrop = Backdrop::new(c, self.hwnd.get())?;

        let surface = self.gfx.surface(1, 1)?;
        let content = c.CreateSpriteVisual()?;
        content.SetBrush(&c.CreateSurfaceBrushWithSurface(&surface)?)?;

        root.Children()?.InsertAtTop(&backdrop.blur)?;
        root.Children()?.InsertAtTop(&backdrop.tint_visual)?;
        root.Children()?.InsertAtTop(&content)?;
        target.SetRoot(&root)?;
        *self.comp.borrow_mut() = Some(Comp {
            _target: target,
            backdrop,
            content,
            surface,
        });
        self.apply_background();
        Ok(())
    }

    fn apply_background(&self) {
        let bar = self.state.borrow().palette.bar;
        if let Some(c) = self.comp.borrow().as_ref() {
            c.backdrop.apply(bar);
        }
    }

    fn is_main(&self) -> bool {
        self.area.get().is_none()
    }

    /// One mirror bar per extra monitor: created when a monitor appears, hidden
    /// (and its screen edge given back) when it goes. Only the main bar runs this.
    fn sync_monitors(&'static self) {
        if self.hidden || !self.is_main() {
            return;
        }
        let rects = window::other_monitors();
        let have = OTHERS.with(|o| o.borrow().len());
        for _ in have..rects.len() {
            match self.create_other() {
                Ok(b) => OTHERS.with(|o| o.borrow_mut().push(b)),
                Err(e) => return log::error(&format!("menubar on other monitor: {e}")),
            }
        }
        let others: Vec<&'static MenuBar> = OTHERS.with(|o| o.borrow().clone());
        for (i, b) in others.iter().enumerate() {
            match rects.get(i) {
                Some(rc) => {
                    b.area.set(Some(*rc));
                    if !b.registered.replace(true) {
                        appbar::register(b.hwnd.get(), WM_APPBAR);
                    }
                    b.size.set((0, 0));
                    b.place();
                    unsafe { _ = ShowWindow(b.hwnd.get(), SW_SHOWNOACTIVATE) };
                }
                None => {
                    unsafe { _ = ShowWindow(b.hwnd.get(), SW_HIDE) };
                    if b.registered.replace(false) {
                        appbar::remove(b.hwnd.get());
                    }
                }
            }
        }
        self.mirror();
    }

    fn create_other(&self) -> Result<&'static MenuBar> {
        let bar: &'static MenuBar = Box::leak(Box::new(MenuBar {
            hwnd: Cell::new(HWND::default()),
            fonts: Fonts::new(&self.gfx.dwrite)?,
            gfx: self.gfx.clone(),
            hidden: false,
            dpi: Cell::new(0),
            size: Cell::new((0, 0)),
            state: RefCell::new(self.state.borrow().clone()),
            comp: RefCell::new(None),
            zones: RefCell::new(Vec::new()),
            sources: OnceCell::new(),
            status: Cell::new(self.status.get()),
            placing: Cell::new(false),
            taskbar_created: self.taskbar_created,
            area: Cell::new(Some(RECT::default())),
            registered: Cell::new(false),
        }));
        bar.hwnd.set(window::create(CLASS_OTHER, bar)?);
        bar.init_visuals()?;
        Ok(bar)
    }

    /// Copies the main bar's content (app, clock, icons, colours) to the others.
    fn mirror(&self) {
        if !self.is_main() {
            return;
        }
        let st = self.state.borrow().clone();
        for b in OTHERS.with(|o| o.borrow().clone()) {
            if b.registered.get() {
                if b.state.borrow().palette != st.palette {
                    *b.state.borrow_mut() = st.clone();
                    b.apply_background();
                } else {
                    *b.state.borrow_mut() = st.clone();
                }
                b.redraw();
            }
        }
    }

    /// A full-screen app (game, video, slides) has the monitor: bar and dock
    /// step aside until it leaves. Our own Launchpad and the desktop don't count.
    fn fullscreen(&self, on: bool) {
        if self.hidden {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        if on && (crate::winlist::is_own(fg) || is_desktop(fg)) {
            return;
        }
        unsafe {
            _ = ShowWindow(self.hwnd.get(), if on { SW_HIDE } else { SW_SHOWNOACTIVATE });
            if !self.is_main() {
                return; // the dock lives on the primary monitor
            }
            if let Ok(dock) = FindWindowW(crate::dock::CLASS, None) {
                _ = PostMessageW(Some(dock), crate::dock::WM_FULLSCREEN, WPARAM(on as usize), LPARAM(0));
            }
        }
    }

    /// Sizes and positions the bar along the top of its monitor; redraws if the size or DPI changed.
    fn place(&self) {
        if self.placing.replace(true) {
            return;
        }
        let hwnd = self.hwnd.get();
        unsafe {
            let (monitor, dpi) = match self.area.get() {
                Some(rc) => window::monitor_at(rc.left + 1, rc.top + 1),
                None => window::primary_monitor(),
            };
            let h = theme::px(theme::MENUBAR_HEIGHT, dpi);
            let rc = if self.hidden {
                appbar::edge_rect(monitor, appbar::Edge::Top, h)
            } else {
                appbar::set_edge(hwnd, appbar::Edge::Top, monitor, h)
            };
            let size = (rc.right - rc.left, h);
            _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rc.left,
                rc.top,
                size.0,
                size.1,
                SWP_NOACTIVATE,
            );

            if dpi != self.dpi.get() || size != self.size.get() {
                self.dpi.set(dpi);
                self.size.set(size);
                if let Err(e) = self.resize_surface(size) {
                    log::error(&format!("menubar resize: {e}"));
                }
                self.redraw();
            }
        }
        self.placing.set(false);
    }

    fn resize_surface(&self, (w, h): (i32, i32)) -> Result<()> {
        if let Some(c) = self.comp.borrow().as_ref() {
            self.gfx.resize(&c.surface, w as u32, h as u32)?;
            c.content.SetSize(Vector2 {
                X: w as f32,
                Y: h as f32,
            })?;
        }
        Ok(())
    }

    fn redraw(&self) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let st = self.state.borrow();
        let scale = theme::scale(self.dpi.get());
        let (w, h) = self.size.get();
        let (wd, hd) = (w as f32 / scale, h as f32 / scale);
        if let Err(e) = self.gfx.draw(&c.surface, scale, |dc| {
            *self.zones.borrow_mut() = draw(dc, &self.gfx.dwrite, &self.fonts, &st, wd, hd)?;
            Ok(())
        }) {
            log::error(&format!("menubar draw: {e}"));
        }
        drop((st, comp));
        self.mirror();
    }

    /// Opens the panel or menu of the part under `x` (client px), macOS style.
    fn click(&self, x_px: i32) {
        let s = theme::scale(self.dpi.get());
        let x = x_px as f32 / s;
        let Some(zone) = self.zones.borrow().iter().find(|z| x >= z.left && x < z.right).copied() else {
            return;
        };
        let mut rc = RECT::default();
        unsafe { _ = GetWindowRect(self.hwnd.get(), &mut rc) };
        let centre = rc.left + (((zone.left + zone.right) / 2.0) * s).round() as i32;
        let below = rc.bottom + (4.0 * s).round() as i32;
        let kind = match zone.hit {
            Hit::Logo => return system_menu(rc.left + (14.0 * s) as i32 + (130.0 * s) as i32, below),
            Hit::Clock => panels::Kind::Calendar,
            Hit::Status(status::Kind::Network) => panels::Kind::Wifi,
            Hit::Status(status::Kind::Battery) => panels::Kind::Battery,
            Hit::Status(status::Kind::Volume) => panels::Kind::Volume,
            Hit::Status(status::Kind::Control) => panels::Kind::Control,
            Hit::Tray(i) => return crate::tray::click(i, false, centre, rc.bottom),
        };
        panels::toggle(kind, centre, below);
    }

    /// Tray icon under `x_px` (client px): its index, centre x and the bar's bottom (screen px).
    fn tray_at(&self, x_px: i32) -> Option<(usize, i32, i32)> {
        let s = theme::scale(self.dpi.get());
        let x = x_px as f32 / s;
        let zone = self.zones.borrow().iter().find(|z| x >= z.left && x < z.right).copied()?;
        let Hit::Tray(i) = zone.hit else { return None };
        let mut rc = RECT::default();
        unsafe { _ = GetWindowRect(self.hwnd.get(), &mut rc) };
        Some((i, rc.left + (((zone.left + zone.right) / 2.0) * s).round() as i32, rc.bottom))
    }

    /// Right click on the bar: the settings people look for there.
    fn bar_menu(&self) {
        use windows::Win32::UI::Shell::ShellExecuteW;
        let s = theme::scale(self.dpi.get());
        let mut rc = RECT::default();
        let mut pt = POINT::default();
        unsafe {
            _ = GetWindowRect(self.hwnd.get(), &mut rc);
            _ = GetCursorPos(&mut pt);
        }
        let entries = vec![
            menu::Entry::item(1, "Masaüstü ve Dock ayarları…"),
            menu::Entry::item(2, "Tarih ve saat ayarları…"),
            menu::Entry::item(3, "Bildirimler…"),
            menu::Entry::item(4, "Görev Yöneticisi…"),
            menu::Entry::Separator,
            menu::Entry::item(5, "Menü çubuğu ve Dock'tan çık"),
        ];
        let open = |what: &str| unsafe {
            ShellExecuteW(None, w!("open"), &windows::core::HSTRING::from(what), None, None, SW_SHOWNORMAL);
        };
        menu::show(entries, pt.x, popup::Place::Below(rc.bottom + (4.0 * s) as i32), move |id| match id {
            1 => crate::prefs::open(),
            2 => open("ms-settings:dateandtime"),
            3 => open("ms-actioncenter:"),
            4 => open("taskmgr.exe"),
            5 => crate::quit_running(),
            _ => {}
        });
    }

    fn refresh_clock(&self) {
        let now = clock::now();
        self.state.borrow_mut().clock = clock::text(&now);
        self.redraw();
        // +20 ms so a timer firing a tick early still lands in the new minute.
        let delay = clock::ms_to_next_minute(now.wSecond, now.wMilliseconds) + 20;
        unsafe { SetTimer(Some(self.hwnd.get()), TIMER_CLOCK, delay, None) };
    }

    /// Shows the app owning `fg`, unless it is a shell surface (Start, search).
    fn set_app(&self, fg: HWND) {
        let Some(name) = appid::display_name(fg) else {
            return;
        };
        if self.state.borrow().app_name == name {
            return;
        }
        self.state.borrow_mut().app_name = name;
        self.redraw();
    }

    /// Re-reads only the source that changed (a volume drag fires dozens of events).
    fn refresh_status(&self, what: usize) {
        let Some(sources) = self.sources.get() else {
            return;
        };
        let mut st = self.status.get();
        match what {
            status::VOLUME => st.volume = sources.read_volume(),
            status::NETWORK => st.net = status::read_net(),
            status::AUDIO_DEVICE => {
                sources.bind_audio();
                st.volume = sources.read_volume();
            }
            _ => st.battery = status::read_battery(),
        }
        self.set_status(st);
    }

    /// Redraws only when an icon actually changes.
    fn set_status(&self, st: status::Status) {
        self.status.set(st);
        let icons = status::icons(&st, crate::settings::get().battery_percent);
        if self.state.borrow().status == icons {
            return;
        }
        self.state.borrow_mut().status = icons;
        self.redraw();
    }

    /// Follows a settings change (battery percentage on or off).
    pub fn apply(&self, before: &crate::settings::Settings, after: &crate::settings::Settings) {
        if before.battery_percent != after.battery_percent {
            self.set_status(self.status.get());
        }
    }

    /// Theme or transparency setting changed (both arrive as "ImmersiveColorSet").
    fn refresh_palette(&self) {
        let palette = theme::system_palette();
        let changed = self.state.borrow().palette != palette;
        self.state.borrow_mut().palette = palette;
        self.apply_background();
        if changed {
            self.redraw();
        }
    }
}

impl Handler for MenuBar {
    fn handle(&self, _: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TIMER if wp.0 == TIMER_CLOCK => self.refresh_clock(),
            WM_LBUTTONUP => self.click((lp.0 & 0xFFFF) as i16 as i32),
            WM_RBUTTONUP => {
                if let Some((i, centre, bottom)) = self.tray_at((lp.0 & 0xFFFF) as i16 as i32) {
                    crate::tray::click(i, true, centre, bottom);
                } else {
                    self.bar_menu();
                }
            }
            WM_TRAY => {
                self.state.borrow_mut().tray = crate::tray::icons();
                self.redraw();
            }
            WM_TIMECHANGE => self.refresh_clock(),
            WM_FOREGROUND => self.set_app(HWND(wp.0 as *mut _)),
            status::WM_STATUS => self.refresh_status(wp.0),
            WM_POWERBROADCAST => {
                self.refresh_status(usize::MAX); // battery
                return Some(LRESULT(1));
            }
            WM_APPBAR if wp.0 as u32 == ABN_POSCHANGED => self.place(),
            WM_APPBAR if wp.0 as u32 == ABN_FULLSCREENAPP => self.fullscreen(lp.0 != 0),
            WM_DPICHANGED => self.place(),
            WM_DISPLAYCHANGE => {
                self.place();
                if self.is_main() {
                    let me: &'static MenuBar = unsafe { &*(self as *const MenuBar) };
                    me.sync_monitors();
                }
            }
            WM_SETTINGCHANGE if !self.is_main() => return None, // the main bar mirrors its colours
            WM_SETTINGCHANGE => {
                let what = if lp.0 == 0 {
                    String::new()
                } else {
                    unsafe { PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default() }
                };
                match what.as_str() {
                    "ImmersiveColorSet" => self.refresh_palette(),
                    "intl" => self.refresh_clock(),
                    _ => return None,
                }
            }
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_CLOSE => unsafe { _ = DestroyWindow(self.hwnd.get()) },
            WM_DESTROY if !self.is_main() => {}
            WM_DESTROY => {
                if !self.hidden {
                    appbar::remove(self.hwnd.get());
                    for b in OTHERS.with(|o| o.borrow().clone()) {
                        if b.registered.replace(false) {
                            appbar::remove(b.hwnd.get());
                        }
                    }
                }
                unsafe { PostQuitMessage(0) };
            }
            m if m == self.taskbar_created && !self.hidden && self.registered.get() => {
                // Explorer restarted and forgot every AppBar.
                appbar::register(self.hwnd.get(), WM_APPBAR);
                self.size.set((0, 0));
                self.place();
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// Out-of-context hook: runs on our thread from the message loop. Posts, so the
/// name lookup happens in normal dispatch and never re-enters a handler.
unsafe extern "system" fn on_foreground(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    id_object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if id_object == OBJID_WINDOW.0 && !hwnd.is_invalid() {
        unsafe {
            _ = PostMessageW(
                Some(BAR_HWND.get()),
                WM_FOREGROUND,
                WPARAM(hwnd.0 as usize),
                LPARAM(0),
            )
        };
    }
}

/// Windows logo menu, in place of the Apple menu: about, settings, power.
/// Power actions ask again in a second menu (no dialog, no sound).
fn system_menu(x: i32, y: i32) {
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::LockWorkStation;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::core::HSTRING;
    let open = |what: &str, args: Option<&str>, show: SHOW_WINDOW_CMD| unsafe {
        let args = HSTRING::from(args.unwrap_or(""));
        ShellExecuteW(None, w!("open"), &HSTRING::from(what), &args, None, show);
    };
    let entries = vec![
        menu::Entry::item(1, "Bu bilgisayar hakkında"),
        menu::Entry::Separator,
        menu::Entry::item(2, "Sistem Ayarları…"),
        menu::Entry::item(3, "Dock ayarları…"),
        menu::Entry::item(4, "Görev Yöneticisi…"),
        menu::Entry::Separator,
        menu::Entry::item(5, "Uyku"),
        menu::Entry::item(6, "Yeniden başlat…"),
        menu::Entry::item(7, "Kapat…"),
        menu::Entry::Separator,
        menu::Entry::item(8, "Ekranı kilitle"),
        menu::Entry::item(9, "Oturumu kapat…"),
        menu::Entry::Separator,
        menu::Entry::item(10, "Menü çubuğu ve Dock'tan çık"),
    ];
    menu::show(entries, x, popup::Place::Below(y), move |id| match id {
        1 => open("ms-settings:about", None, SW_SHOWNORMAL),
        2 => open("ms-settings:", None, SW_SHOWNORMAL),
        3 => crate::prefs::open(),
        4 => open("taskmgr.exe", None, SW_SHOWNORMAL),
        5 => unsafe {
            _ = SetSuspendState(false, false, false);
        },
        6 => confirm("Yeniden başlat", x, y, move || open("shutdown.exe", Some("/r /t 0"), SW_HIDE)),
        7 => confirm("Kapat", x, y, move || open("shutdown.exe", Some("/s /t 0"), SW_HIDE)),
        8 => unsafe {
            _ = LockWorkStation();
        },
        9 => confirm("Oturumu kapat", x, y, move || open("shutdown.exe", Some("/l"), SW_HIDE)),
        10 => crate::quit_running(),
        _ => {}
    });
}

/// Second step for actions that end the session: pick it again or cancel.
fn confirm(action: &'static str, x: i32, y: i32, run: impl FnOnce() + 'static) {
    let entries = vec![menu::Entry::item(1, format!("{action}: emin misiniz?")), menu::Entry::Separator, menu::Entry::item(2, action), menu::Entry::item(0, "Vazgeç")];
    menu::show(entries, x, popup::Place::Below(y), move |id| {
        if id == 2 {
            run();
        }
    });
}

/// Desktop windows (wallpaper host): clicking the desktop can look "full screen" to the shell.
fn is_desktop(hwnd: HWND) -> bool {
    let mut buf = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    matches!(String::from_utf16_lossy(&buf[..n]).as_str(), "Progman" | "WorkerW")
}
