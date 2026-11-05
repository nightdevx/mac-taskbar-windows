//! Bottom dock: pinned and running apps, one composition visual per icon so
//! effects only move visuals, never redraw. Icon size is adjustable and the
//! dock can hide itself, like the Ubuntu dock.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use windows::Foundation::TimeSpan;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionDrawingSurface, CompositionRoundedRectangleGeometry, CompositionSurfaceBrush, ContainerVisual, SpriteVisual,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{ABN_POSCHANGED, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};
use windows_numerics::{Vector2, Vector3};

use crate::appid::{self, AppIdentity};
use crate::gfx::Gfx;
use crate::icons::{self, Loader};
use crate::settings::{self, Settings};
use crate::theme::{self, Palette};
use crate::window::{self, Backdrop, Handler};
use crate::winlist::{self, ShellEvent};
use crate::{appbar, log};

pub const CLASS: PCWSTR = w!("TaskbarDock");
const WM_APPBAR: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 4;
/// A window's cloaked state changed (Store apps, virtual desktops): re-check it.
const WM_RECHECK: u32 = WM_APP + 5;
/// `--settings` from a second launch asks the running instance to open settings.
pub const WM_OPEN_SETTINGS: u32 = WM_APP + 6;
/// Lives in UI::Controls in windows-rs; not worth the feature for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HIDE: usize = 1;
const TIMER_REGION: usize = 2;
/// Pointer away this long before an auto-hiding dock slides out.
const HIDE_DELAY_MS: u32 = 500;
const SLIDE_MS: u32 = 250;
/// Height of the screen-edge strip that brings a hidden dock back (px).
const TRIGGER_PX: i32 = 2;

thread_local! {
    static DOCK_HWND: Cell<HWND> = Cell::new(HWND::default());
}

// ---- layout (DIPs) ----

/// Dock geometry for one icon size; everything else scales from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub icon: f32,
}

impl Geometry {
    pub fn pad(self) -> f32 {
        (self.icon / 8.0).round()
    }
    pub fn gap(self) -> f32 {
        self.pad()
    }
    /// Below the icons: room for the running dot.
    pub fn pad_bottom(self) -> f32 {
        self.pad() + theme::DOCK_DOT
    }
    pub fn radius(self) -> f32 {
        (self.icon * 0.375).round()
    }
    pub fn width(self, n: usize) -> f32 {
        let n = n.max(1) as f32;
        2.0 * self.pad() + n * self.icon + (n - 1.0) * self.gap()
    }
    pub fn height(self) -> f32 {
        self.pad() + self.icon + self.pad_bottom()
    }
    pub fn item_x(self, i: usize) -> f32 {
        self.pad() + i as f32 * (self.icon + self.gap())
    }
    /// Item under `x`; each gap is split between its two neighbours.
    pub fn hit(self, n: usize, x: f32) -> Option<usize> {
        let pos = x - self.pad() + self.gap() / 2.0;
        if pos < 0.0 {
            return None;
        }
        let i = (pos / (self.icon + self.gap())) as usize;
        (i < n).then_some(i)
    }
}

// ---- model ----

struct Item {
    identity: AppIdentity,
    /// Pin source when pinned (.lnk, exe or `shell:AppsFolder\<AUMID>`).
    pin: Option<String>,
    windows: Vec<HWND>,
    /// Parsing name for the icon and for launching.
    source: String,
    icon: SpriteVisual,
    dot: SpriteVisual,
}

/// Where the icon of a running, unpinned app comes from.
fn source_for(id: &AppIdentity) -> Option<String> {
    match (&id.aumid, &id.exe) {
        // Package AUMIDs ("Pkg_hash!App") have the proper Start menu icon.
        (Some(a), _) if a.contains('!') => Some(format!(r"shell:AppsFolder\{a}")),
        (_, Some(exe)) => Some(exe.clone()),
        (Some(a), None) => Some(format!(r"shell:AppsFolder\{a}")),
        (None, None) => None,
    }
}

/// The Windows taskbar's own pins, used until the user changes pins in the dock.
fn windows_pins() -> Vec<String> {
    let Some(appdata) = std::env::var_os("APPDATA") else { return Vec::new() };
    let dir = std::path::Path::new(&appdata).join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let mut links: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))).map(|p| p.to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    // ponytail: name order; the Windows taskbar's own order lives in an undocumented registry blob.
    links.sort_by_key(|p| p.to_lowercase());
    links
}

/// A dock entry before it has visuals: what `entries()` finds at start.
pub struct Entry {
    pub identity: AppIdentity,
    pub pin: Option<String>,
    pub windows: Vec<HWND>,
    pub source: String,
}

/// Pinned apps, then running apps that are not pinned, oldest window first.
pub fn entries(settings: &Settings) -> Vec<Entry> {
    let pins = settings.pins.clone().unwrap_or_else(windows_pins);
    let mut out: Vec<Entry> = pins
        .into_iter()
        .filter_map(|p| Some(Entry { identity: appid::source_identity(&p)?, pin: Some(p.clone()), windows: Vec::new(), source: p }))
        .collect();
    for hwnd in winlist::enumerate().into_iter().rev().filter(|h| !winlist::is_own(*h)) {
        let Some(identity) = appid::window_identity(hwnd) else { continue };
        match out.iter_mut().find(|e| e.identity.same_app(&identity)) {
            Some(e) => e.windows.push(hwnd),
            None => {
                if let Some(source) = source_for(&identity) {
                    out.push(Entry { identity, pin: None, windows: vec![hwnd], source });
                }
            }
        }
    }
    out
}

struct Comp {
    _target: DesktopWindowTarget,
    /// Everything that slides when the dock hides.
    slide: ContainerVisual,
    backdrop: Backdrop,
    shape: CompositionRoundedRectangleGeometry,
    items: ContainerVisual,
    dot_surface: CompositionDrawingSurface,
    dot_brush: CompositionSurfaceBrush,
}

pub struct Dock {
    hwnd: Cell<HWND>,
    gfx: Rc<Gfx>,
    hidden: bool,
    dpi: Cell<u32>,
    /// Bottom strip of the screen the dock sits in (granted by the AppBar when not auto-hiding).
    strip: Cell<RECT>,
    /// Window size in px.
    size: Cell<(i32, i32)>,
    palette: Cell<Palette>,
    /// Auto-hide: whether the dock is currently out.
    shown: Cell<bool>,
    tracking: Cell<bool>,
    registered: Cell<bool>,
    items: RefCell<Vec<Item>>,
    comp: RefCell<Option<Comp>>,
    brushes: RefCell<HashMap<String, CompositionSurfaceBrush>>,
    requested: RefCell<HashSet<String>>,
    loader: RefCell<Option<Loader>>,
    placing: Cell<bool>,
    shellhook: u32,
    taskbar_created: u32,
}

impl Dock {
    pub fn create(gfx: Rc<Gfx>, hidden: bool) -> Result<&'static Dock> {
        let dock: &'static Dock = Box::leak(Box::new(Dock {
            hwnd: Cell::new(HWND::default()),
            gfx,
            hidden,
            dpi: Cell::new(96),
            strip: Cell::new(RECT::default()),
            size: Cell::new((0, 0)),
            palette: Cell::new(theme::system_palette()),
            shown: Cell::new(true),
            tracking: Cell::new(false),
            registered: Cell::new(false),
            items: RefCell::new(Vec::new()),
            comp: RefCell::new(None),
            brushes: RefCell::new(HashMap::new()),
            requested: RefCell::new(HashSet::new()),
            loader: RefCell::new(None),
            placing: Cell::new(false),
            shellhook: unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) },
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        }));
        let hwnd = window::create(CLASS, dock)?;
        dock.hwnd.set(hwnd);
        DOCK_HWND.set(hwnd);
        *dock.loader.borrow_mut() = Some(Loader::start(hwnd, WM_ICON));
        dock.init_visuals()?;
        dock.sync_appbar();
        dock.place();

        let entries = entries(&settings::get());
        for e in entries {
            if let Some(i) = dock.add_item(e.identity, e.pin, e.source) {
                dock.items.borrow_mut()[i].windows = e.windows;
            }
        }
        unsafe {
            _ = RegisterShellHookWindow(hwnd);
            SetWinEventHook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED, None, Some(on_cloak), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        }
        dock.relayout();
        if !hidden {
            unsafe { _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        }
        if dock.autohide() {
            dock.slide_out();
        }
        log::info(&format!("dock: {} items", dock.items.borrow().len()));
        Ok(dock)
    }

    fn scale(&self) -> f32 {
        theme::scale(self.dpi.get())
    }

    fn geometry(&self) -> Geometry {
        Geometry { icon: settings::get().dock_icon }
    }

    fn autohide(&self) -> bool {
        settings::get().dock_autohide
    }

    fn init_visuals(&self) -> Result<()> {
        let c = &self.gfx.compositor;
        let target = self.gfx.target(self.hwnd.get())?;
        let root = c.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        // Offset changes on `slide` animate on their own (DWM runs the animation).
        let slide = c.CreateContainerVisual()?;
        slide.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let anim = c.CreateVector3KeyFrameAnimation()?;
        anim.SetTarget(&HSTRING::from("Offset"))?;
        let ease_out = c.CreateCubicBezierEasingFunction(Vector2 { X: 0.2, Y: 0.0 }, Vector2 { X: 0.0, Y: 1.0 })?;
        anim.InsertExpressionKeyFrameWithEasingFunction(1.0, &HSTRING::from("this.FinalValue"), &ease_out)?;
        anim.SetDuration(TimeSpan { Duration: SLIDE_MS as i64 * 10_000 })?;
        let implicit = c.CreateImplicitAnimationCollection()?;
        implicit.Insert(&HSTRING::from("Offset"), &anim)?;
        slide.SetImplicitAnimations(&implicit)?;

        // Rounded frame clips the blur and tint; icons sit above it unclipped (room for effects).
        let frame = c.CreateContainerVisual()?;
        let shape = c.CreateRoundedRectangleGeometry()?;
        frame.SetClip(&c.CreateGeometricClipWithGeometry(&shape)?)?;
        frame.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let backdrop = Backdrop::new(c, self.hwnd.get())?;
        frame.Children()?.InsertAtTop(&backdrop.blur)?;
        frame.Children()?.InsertAtTop(&backdrop.tint_visual)?;

        let items = c.CreateContainerVisual()?;
        let dot_surface = self.gfx.surface(1, 1)?;
        let dot_brush = c.CreateSurfaceBrushWithSurface(&dot_surface)?;

        slide.Children()?.InsertAtTop(&frame)?;
        slide.Children()?.InsertAtTop(&items)?;
        root.Children()?.InsertAtTop(&slide)?;
        target.SetRoot(&root)?;
        backdrop.apply(self.palette.get().bar);
        *self.comp.borrow_mut() = Some(Comp { _target: target, slide, backdrop, shape, items, dot_surface, dot_brush });
        Ok(())
    }

    /// Reserved screen strip only when the dock stays visible; an auto-hiding
    /// dock leaves the whole screen to apps and floats over them.
    fn sync_appbar(&self) {
        if self.hidden {
            return;
        }
        let want = !self.autohide();
        if want && !self.registered.get() {
            appbar::register(self.hwnd.get(), WM_APPBAR);
        } else if !want && self.registered.get() {
            appbar::remove(self.hwnd.get());
        }
        self.registered.set(want);
    }

    fn thickness_px(&self) -> i32 {
        theme::px(self.geometry().height() + theme::DOCK_MARGIN, self.dpi.get())
    }

    /// Picks the bottom strip (on start, size, DPI or display change) and repositions.
    fn place(&self) {
        if self.placing.replace(true) {
            return;
        }
        let (monitor, dpi) = window::primary_monitor();
        self.dpi.set(dpi);
        let thickness = self.thickness_px();
        let strip = if self.registered.get() {
            appbar::set_edge(self.hwnd.get(), appbar::Edge::Bottom, monitor, thickness)
        } else {
            appbar::edge_rect(monitor, appbar::Edge::Bottom, thickness)
        };
        self.strip.set(strip);
        self.draw_dot();
        self.relayout();
        self.placing.set(false);
    }

    /// Sizes the window to the items and moves every icon visual to its slot.
    /// The window runs down to the screen edge: the margin below the dock is the
    /// auto-hide trigger strip.
    fn relayout(&self) {
        let s = self.scale();
        let g = self.geometry();
        let items = self.items.borrow();
        let w = (g.width(items.len()) * s).round() as i32;
        let h = self.thickness_px();
        let strip = self.strip.get();
        let x = (strip.left + strip.right) / 2 - w / 2;
        unsafe { _ = SetWindowPos(self.hwnd.get(), Some(HWND_TOPMOST), x, strip.bottom - h, w, h, SWP_NOACTIVATE) };
        self.size.set((w, h));

        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        _ = c.shape.SetSize(Vector2 { X: w as f32, Y: (g.height() * s).round() });
        _ = c.shape.SetCornerRadius(Vector2 { X: g.radius() * s, Y: g.radius() * s });
        let icon = (g.icon * s).round();
        let dot = theme::DOCK_DOT * s;
        let indicators = settings::get().dock_indicators;
        for (i, item) in items.iter().enumerate() {
            let left = (g.item_x(i) * s).round();
            _ = item.icon.SetOffset(Vector3 { X: left, Y: (g.pad() * s).round(), Z: 0.0 });
            _ = item.icon.SetSize(Vector2 { X: icon, Y: icon });
            let dot_y = (g.pad() + g.icon + (g.pad_bottom() - theme::DOCK_DOT) / 2.0) * s;
            _ = item.dot.SetOffset(Vector3 { X: (left + (icon - dot) / 2.0).round(), Y: dot_y.round(), Z: 0.0 });
            _ = item.dot.SetSize(Vector2 { X: dot, Y: dot });
            _ = item.dot.SetIsVisible(indicators && !item.windows.is_empty());
        }
        drop(comp);
        drop(items);
        if !self.shown.get() {
            self.set_trigger_region();
        }
    }

    // ---- auto-hide ----

    fn slide_to(&self, out: bool) {
        let y = if out { self.size.get().1 as f32 } else { 0.0 };
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.slide.SetOffset(Vector3 { X: 0.0, Y: y, Z: 0.0 });
        }
    }

    fn slide_out(&self) {
        self.shown.set(false);
        self.slide_to(true);
        // Clip the window to the trigger strip once the slide has finished, so apps get the clicks.
        unsafe { SetTimer(Some(self.hwnd.get()), TIMER_REGION, SLIDE_MS + 30, None) };
    }

    fn slide_in(&self) {
        unsafe {
            _ = KillTimer(Some(self.hwnd.get()), TIMER_REGION);
            SetWindowRgn(self.hwnd.get(), None, true);
        }
        self.shown.set(true);
        self.slide_to(false);
    }

    fn set_trigger_region(&self) {
        let (w, h) = self.size.get();
        unsafe { SetWindowRgn(self.hwnd.get(), Some(CreateRectRgn(0, h - TRIGGER_PX, w, h)), true) };
    }

    fn on_mouse_move(&self) {
        if !self.tracking.replace(true) {
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd.get(), dwHoverTime: 0 };
            unsafe { _ = TrackMouseEvent(&mut t) };
        }
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_HIDE) };
        if self.autohide() && !self.shown.get() {
            self.slide_in();
        }
    }

    fn on_mouse_leave(&self) {
        self.tracking.set(false);
        if self.autohide() && self.shown.get() {
            unsafe { SetTimer(Some(self.hwnd.get()), TIMER_HIDE, HIDE_DELAY_MS, None) };
        }
    }

    // ---- icons and items ----

    /// Running-app dot, drawn once per DPI or theme and shared by every item.
    fn draw_dot(&self) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let s = self.scale();
        let px = (theme::DOCK_DOT * s).ceil() as u32;
        let color = self.palette.get().text;
        let r = self.gfx.resize(&c.dot_surface, px, px).and_then(|_| {
            self.gfx.draw(&c.dot_surface, s, |dc| unsafe {
                let brush = dc.CreateSolidColorBrush(&color.d2d(), None)?;
                let r = theme::DOCK_DOT / 2.0;
                dc.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: r, Y: r }, radiusX: r, radiusY: r }, &brush);
                Ok(())
            })
        });
        if let Err(e) = r {
            log::error(&format!("dock dot: {e}"));
        }
    }

    fn add_item(&self, identity: AppIdentity, pin: Option<String>, source: String) -> Option<usize> {
        let comp = self.comp.borrow();
        let c = comp.as_ref()?;
        let compositor = &self.gfx.compositor;
        let icon = compositor.CreateSpriteVisual().ok()?;
        let dot = compositor.CreateSpriteVisual().ok()?;
        _ = dot.SetBrush(&c.dot_brush);
        _ = c.items.Children().and_then(|ch| ch.InsertAtTop(&icon));
        _ = c.items.Children().and_then(|ch| ch.InsertAtTop(&dot));
        drop(comp);
        self.set_icon(&icon, &source);
        let mut items = self.items.borrow_mut();
        items.push(Item { identity, pin, windows: Vec::new(), source, icon, dot });
        Some(items.len() - 1)
    }

    fn remove_item(&self, index: usize) {
        let item = self.items.borrow_mut().remove(index);
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.icon));
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.dot));
        }
    }

    /// Uses the cached icon for `source`, or asks the worker for it. Icons are
    /// extracted once at the largest size and magnification, then scaled down.
    fn set_icon(&self, visual: &SpriteVisual, source: &str) {
        if let Some(brush) = self.brushes.borrow().get(source) {
            _ = visual.SetBrush(brush);
            return;
        }
        if self.requested.borrow_mut().insert(source.to_string())
            && let Some(loader) = self.loader.borrow().as_ref()
        {
            loader.request(source, (settings::DOCK_ICON_MAX * theme::DOCK_MAGNIFY * self.scale()).round() as u32);
        }
    }

    fn on_icon(&self, icon: Box<icons::Icon>) {
        if icon.pixels.is_empty() {
            log::error(&format!("no icon for {}", icon.source));
            return;
        }
        let brush = match self.icon_brush(&icon) {
            Ok(b) => b,
            Err(e) => return log::error(&format!("icon surface: {e}")),
        };
        for item in self.items.borrow().iter().filter(|i| i.source == icon.source) {
            _ = item.icon.SetBrush(&brush);
        }
        self.brushes.borrow_mut().insert(icon.source.clone(), brush);
    }

    fn icon_brush(&self, icon: &icons::Icon) -> Result<CompositionSurfaceBrush> {
        let surface = self.gfx.surface(icon.size, icon.size)?;
        self.gfx.draw(&surface, 1.0, |dc| unsafe {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let bmp = dc.CreateBitmap(D2D_SIZE_U { width: icon.size, height: icon.size }, Some(icon.pixels.as_ptr() as *const _), icon.size * 4, &props)?;
            let side = icon.size as f32;
            dc.DrawBitmap(&bmp, Some(&D2D_RECT_F { left: 0.0, top: 0.0, right: side, bottom: side }), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
            Ok(())
        })?;
        self.gfx.compositor.CreateSurfaceBrushWithSurface(&surface)
    }

    fn tracks(&self, hwnd: HWND) -> bool {
        self.items.borrow().iter().any(|i| i.windows.contains(&hwnd))
    }

    fn add_window(&self, hwnd: HWND) {
        if self.tracks(hwnd) || !winlist::qualifies(hwnd) || winlist::is_own(hwnd) {
            return;
        }
        let Some(identity) = appid::window_identity(hwnd) else { return };
        let found = self.items.borrow().iter().position(|i| i.identity.same_app(&identity));
        let index = match found {
            Some(i) => i,
            None => {
                let Some(source) = source_for(&identity) else { return };
                match self.add_item(identity, None, source) {
                    Some(i) => i,
                    None => return,
                }
            }
        };
        self.items.borrow_mut()[index].windows.push(hwnd);
        self.relayout();
    }

    fn remove_window(&self, hwnd: HWND) {
        for item in self.items.borrow_mut().iter_mut() {
            item.windows.retain(|w| *w != hwnd);
        }
        let empty = self.items.borrow().iter().position(|i| i.pin.is_none() && i.windows.is_empty());
        if let Some(i) = empty {
            self.remove_item(i);
        }
        self.relayout();
    }

    fn on_shell(&self, event: ShellEvent) {
        match event {
            ShellEvent::Created(h) | ShellEvent::Activated(h) | ShellEvent::Redraw(h) => self.add_window(h),
            ShellEvent::Destroyed(h) | ShellEvent::Replaced(h) => self.remove_window(h),
            ShellEvent::Flash(_) | ShellEvent::Other => {}
        }
    }

    fn recheck(&self, hwnd: HWND) {
        if winlist::qualifies(hwnd) {
            self.add_window(hwnd);
        } else if self.tracks(hwnd) {
            self.remove_window(hwnd);
        }
    }

    fn item_at(&self, x_px: i32) -> Option<usize> {
        self.geometry().hit(self.items.borrow().len(), x_px as f32 / self.scale())
    }

    /// macOS click: start the app, bring its window forward, minimise it when it
    /// is already in front, or cycle through several windows.
    fn click(&self, x_px: i32, new_instance: bool) {
        let Some(i) = self.item_at(x_px) else { return };
        let (windows, launch) = {
            let items = self.items.borrow();
            let item = &items[i];
            (item.windows.clone(), item.pin.clone().unwrap_or_else(|| item.source.clone()))
        };
        if windows.is_empty() || new_instance {
            unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(launch), None, None, SW_SHOWNORMAL) };
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        let target = match windows.iter().position(|w| *w == fg) {
            Some(_) if windows.len() == 1 => {
                unsafe { _ = ShowWindow(fg, SW_MINIMIZE) };
                return;
            }
            Some(p) => windows[(p + 1) % windows.len()],
            None => windows[0],
        };
        unsafe {
            if IsIconic(target).as_bool() {
                _ = ShowWindow(target, SW_RESTORE);
            }
            _ = SetForegroundWindow(target);
        }
    }

    // ---- context menu ----

    fn context_menu(&self, x_px: i32) {
        const KEEP: usize = 1;
        const CLOSE: usize = 2;
        const AUTOHIDE: usize = 3;
        const SETTINGS: usize = 4;
        let index = self.item_at(x_px);
        let (pinned, windows) = index.map_or((false, Vec::new()), |i| {
            let items = self.items.borrow();
            (items[i].pin.is_some(), items[i].windows.clone())
        });
        let autohide = self.autohide();

        let chosen = unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            if index.is_some() {
                _ = AppendMenuW(menu, MF_STRING, KEEP, if pinned { w!("Dock'tan kaldır") } else { w!("Dock'ta tut") });
                if !windows.is_empty() {
                    _ = AppendMenuW(menu, MF_STRING, CLOSE, if windows.len() > 1 { w!("Tüm pencereleri kapat") } else { w!("Kapat") });
                }
                _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            // macOS wording: the menu names the action, settings hold the rest.
            _ = AppendMenuW(menu, MF_STRING, AUTOHIDE, if autohide { w!("Gizlemeyi kapat") } else { w!("Gizlemeyi aç") });
            _ = AppendMenuW(menu, MF_STRING, SETTINGS, w!("Dock ayarları…"));

            let mut pt = POINT::default();
            _ = GetCursorPos(&mut pt);
            _ = SetForegroundWindow(self.hwnd.get()); // or the menu does not close on an outside click
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_CENTERALIGN, pt.x, pt.y, None, self.hwnd.get(), None);
            _ = PostMessageW(Some(self.hwnd.get()), WM_NULL, WPARAM(0), LPARAM(0));
            _ = DestroyMenu(menu);
            cmd.0 as usize
        };

        match chosen {
            KEEP => {
                if let Some(i) = index {
                    self.toggle_pin(i);
                }
            }
            CLOSE => windows.iter().for_each(|w| unsafe { _ = PostMessageW(Some(*w), WM_CLOSE, WPARAM(0), LPARAM(0)) }),
            AUTOHIDE => settings::update(|st| st.dock_autohide = !st.dock_autohide),
            SETTINGS => crate::prefs::open(),
            _ => {}
        }
        // The menu swallowed the mouse-leave; hide again if the pointer went elsewhere.
        if self.autohide() && !self.pointer_inside() {
            self.on_mouse_leave();
        }
    }

    fn pointer_inside(&self) -> bool {
        let mut pt = POINT::default();
        let mut rc = RECT::default();
        unsafe {
            _ = GetCursorPos(&mut pt);
            _ = GetWindowRect(self.hwnd.get(), &mut rc);
        }
        pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
    }

    /// Keeps or removes the app at `index`; the first change copies the
    /// Windows taskbar pins into our own list.
    fn toggle_pin(&self, index: usize) {
        let (pin, source, windows_empty) = {
            let items = self.items.borrow();
            (items[index].pin.clone(), items[index].source.clone(), items[index].windows.is_empty())
        };
        match pin {
            Some(_) => {
                self.items.borrow_mut()[index].pin = None;
                if windows_empty {
                    self.remove_item(index);
                }
            }
            None => self.items.borrow_mut()[index].pin = Some(source),
        }
        let pins: Vec<String> = self.items.borrow().iter().filter_map(|i| i.pin.clone()).collect();
        settings::update(|st| st.pins = Some(pins));
    }

    /// Follows a settings change (from the menu or the settings window).
    pub fn apply(&self, before: &Settings, after: &Settings) {
        let layout = (after.dock_icon, after.dock_autohide, after.dock_indicators) != (before.dock_icon, before.dock_autohide, before.dock_indicators);
        if !layout {
            return;
        }
        if after.dock_autohide != before.dock_autohide {
            self.sync_appbar();
        }
        self.place();
        if after.dock_autohide && !before.dock_autohide {
            self.slide_out();
        } else if !after.dock_autohide && before.dock_autohide {
            self.slide_in();
        }
    }

    fn refresh_palette(&self) {
        let palette = theme::system_palette();
        let changed = self.palette.replace(palette) != palette;
        if let Some(c) = self.comp.borrow().as_ref() {
            c.backdrop.apply(palette.bar);
        }
        if changed {
            self.draw_dot();
        }
    }
}

impl Handler for Dock {
    fn handle(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let x = || (lp.0 & 0xFFFF) as i16 as i32;
        match msg {
            WM_OPEN_SETTINGS => crate::prefs::open(),
            m if m == self.shellhook && m != 0 => self.on_shell(winlist::decode(wp.0, lp.0)),
            WM_ICON => self.on_icon(unsafe { icons::take(lp) }),
            WM_RECHECK => self.recheck(HWND(wp.0 as *mut _)),
            WM_MOUSEMOVE => self.on_mouse_move(),
            WM_MOUSELEAVE => self.on_mouse_leave(),
            WM_TIMER if wp.0 == TIMER_HIDE => unsafe {
                _ = KillTimer(Some(hwnd), TIMER_HIDE);
                if self.autohide() {
                    self.slide_out();
                }
            },
            WM_TIMER if wp.0 == TIMER_REGION => unsafe {
                _ = KillTimer(Some(hwnd), TIMER_REGION);
                if !self.shown.get() {
                    self.set_trigger_region();
                }
            },
            WM_LBUTTONUP => self.click(x(), false),
            WM_MBUTTONUP => self.click(x(), true),
            WM_RBUTTONUP => self.context_menu(x()),
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_APPBAR if wp.0 as u32 == ABN_POSCHANGED => self.place(),
            WM_DPICHANGED | WM_DISPLAYCHANGE => self.place(),
            WM_SETTINGCHANGE => {
                if lp.0 == 0 || unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default() != "ImmersiveColorSet" {
                    return None;
                }
                self.refresh_palette();
            }
            WM_CLOSE => unsafe { _ = DestroyWindow(hwnd) },
            WM_DESTROY => {
                if self.registered.get() {
                    appbar::remove(hwnd);
                }
                unsafe { _ = DeregisterShellHookWindow(hwnd) };
            }
            m if m == self.taskbar_created && !self.hidden => {
                // Explorer restarted: new taskbar to hide, every AppBar forgotten.
                crate::shelltray::hide();
                self.registered.set(false);
                self.sync_appbar();
                self.place();
            }
            // Logging off: Explorer saves the auto-hide setting, so put the user's back first.
            WM_ENDSESSION if wp.0 != 0 && !self.hidden => crate::shelltray::restore(),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

unsafe extern "system" fn on_cloak(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    if id_object == OBJID_WINDOW.0 && !hwnd.is_invalid() {
        unsafe { _ = PostMessageW(Some(DOCK_HWND.get()), WM_RECHECK, WPARAM(hwnd.0 as usize), LPARAM(0)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Geometry = Geometry { icon: 48.0 };

    #[test]
    fn geometry_scales_with_icon_size() {
        assert_eq!((G.pad(), G.pad_bottom(), G.height(), G.radius()), (6.0, 10.0, 64.0, 18.0));
        let small = Geometry { icon: 32.0 };
        assert_eq!((small.pad(), small.height()), (4.0, 44.0));
        let big = Geometry { icon: 64.0 };
        assert_eq!((big.pad(), big.height()), (8.0, 84.0));
    }

    #[test]
    fn width_grows_by_icon_and_gap() {
        assert_eq!(G.width(1), 2.0 * G.pad() + G.icon);
        assert_eq!(G.width(3) - G.width(2), G.icon + G.gap());
        assert_eq!(G.width(0), G.width(1), "an empty dock keeps one slot");
    }

    #[test]
    fn hit_splits_gaps_between_neighbours() {
        assert_eq!(G.hit(3, G.item_x(0) + 1.0), Some(0));
        assert_eq!(G.hit(3, G.item_x(1) - G.gap() / 2.0 + 0.1), Some(1));
        assert_eq!(G.hit(3, G.item_x(1) - G.gap() / 2.0 - 0.1), Some(0));
        assert_eq!(G.hit(3, G.item_x(2) + G.icon), Some(2));
        assert_eq!(G.hit(3, G.width(3) + 5.0), None);
        assert_eq!(G.hit(3, -1.0), None);
    }

    #[test]
    fn icon_source_prefers_package_then_exe() {
        let id = |a: Option<&str>, e: Option<&str>| AppIdentity { aumid: a.map(Into::into), exe: e.map(Into::into) };
        assert_eq!(source_for(&id(Some("Pkg_x!App"), Some("c:/x.exe"))).unwrap(), r"shell:AppsFolder\Pkg_x!App");
        assert_eq!(source_for(&id(Some("Brave"), Some("c:/brave.exe"))).unwrap(), "c:/brave.exe");
        assert_eq!(source_for(&id(Some("Microsoft.Windows.Explorer"), None)).unwrap(), r"shell:AppsFolder\Microsoft.Windows.Explorer");
        assert!(source_for(&id(None, None)).is_none());
    }
}
