//! Bottom dock: pinned and running apps, one composition visual per icon so
//! later effects (magnification, bounce) only move visuals, never redraw.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionDrawingSurface, CompositionRoundedRectangleGeometry, CompositionSurfaceBrush, ContainerVisual, SpriteVisual,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Shell::{ABN_POSCHANGED, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};
use windows_numerics::{Vector2, Vector3};

use crate::appid::{self, AppIdentity};
use crate::gfx::Gfx;
use crate::icons::{self, Loader};
use crate::theme::{self, Palette};
use crate::window::{self, Backdrop, Handler};
use crate::winlist::{self, ShellEvent};
use crate::{appbar, log};

pub const CLASS: PCWSTR = w!("TaskbarDock");
const WM_APPBAR: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 4;
/// A window's cloaked state changed (Store apps, virtual desktops): re-check it.
const WM_RECHECK: u32 = WM_APP + 5;

thread_local! {
    static DOCK_HWND: Cell<HWND> = Cell::new(HWND::default());
}

// ---- layout (DIPs) ----

pub fn width(n: usize) -> f32 {
    let n = n.max(1) as f32;
    2.0 * theme::DOCK_PAD + n * theme::DOCK_ICON + (n - 1.0) * theme::DOCK_GAP
}

pub fn height() -> f32 {
    theme::DOCK_PAD + theme::DOCK_ICON + theme::DOCK_PAD_BOTTOM
}

pub fn item_x(i: usize) -> f32 {
    theme::DOCK_PAD + i as f32 * (theme::DOCK_ICON + theme::DOCK_GAP)
}

/// Item under `x`; each gap is split between its two neighbours.
pub fn hit(n: usize, x: f32) -> Option<usize> {
    let step = theme::DOCK_ICON + theme::DOCK_GAP;
    let pos = x - theme::DOCK_PAD + theme::DOCK_GAP / 2.0;
    if pos < 0.0 {
        return None;
    }
    let i = (pos / step) as usize;
    (i < n).then_some(i)
}

// ---- model ----

struct Item {
    identity: AppIdentity,
    /// Pinned shortcut (.lnk) path.
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

fn pinned_links() -> Vec<String> {
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

/// Pinned shortcuts, then running apps that are not pinned, oldest window first.
pub fn entries() -> Vec<Entry> {
    let mut out: Vec<Entry> = pinned_links()
        .into_iter()
        .filter_map(|link| Some(Entry { identity: appid::link_identity(&link)?, pin: Some(link.clone()), windows: Vec::new(), source: link }))
        .collect();
    for hwnd in winlist::enumerate().into_iter().rev() {
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
    /// Screen strip granted by the AppBar; the dock floats centred at its bottom.
    strip: Cell<RECT>,
    palette: Cell<Palette>,
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
            palette: Cell::new(theme::system_palette()),
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
        if !hidden {
            appbar::register(hwnd, WM_APPBAR);
        }
        dock.place();

        for e in entries() {
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
        log::info(&format!("dock: {} items", dock.items.borrow().len()));
        Ok(dock)
    }

    fn scale(&self) -> f32 {
        theme::scale(self.dpi.get())
    }

    fn init_visuals(&self) -> Result<()> {
        let c = &self.gfx.compositor;
        let target = self.gfx.target(self.hwnd.get())?;
        let root = c.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        // Rounded frame clips the blur and tint; icons sit above it unclipped (room for F3 effects).
        let frame = c.CreateContainerVisual()?;
        frame.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let shape = c.CreateRoundedRectangleGeometry()?;
        frame.SetClip(&c.CreateGeometricClipWithGeometry(&shape)?)?;
        let backdrop = Backdrop::new(c, self.hwnd.get())?;
        frame.Children()?.InsertAtTop(&backdrop.blur)?;
        frame.Children()?.InsertAtTop(&backdrop.tint_visual)?;

        let items = c.CreateContainerVisual()?;
        let dot_surface = self.gfx.surface(1, 1)?;
        let dot_brush = c.CreateSurfaceBrushWithSurface(&dot_surface)?;

        root.Children()?.InsertAtTop(&frame)?;
        root.Children()?.InsertAtTop(&items)?;
        target.SetRoot(&root)?;
        backdrop.apply(self.palette.get().bar);
        *self.comp.borrow_mut() = Some(Comp { _target: target, backdrop, shape, items, dot_surface, dot_brush });
        Ok(())
    }

    /// Reserves the bottom strip (on start, DPI or display change) and repositions.
    fn place(&self) {
        if self.placing.replace(true) {
            return;
        }
        let (monitor, dpi) = window::primary_monitor();
        let dpi_changed = dpi != self.dpi.get();
        self.dpi.set(dpi);
        let thickness = theme::px(height() + 2.0 * theme::DOCK_MARGIN, dpi);
        let strip = if self.hidden {
            appbar::edge_rect(monitor, appbar::Edge::Bottom, thickness)
        } else {
            appbar::set_edge(self.hwnd.get(), appbar::Edge::Bottom, monitor, thickness)
        };
        self.strip.set(strip);
        if dpi_changed {
            self.draw_dot();
        }
        self.relayout();
        self.placing.set(false);
    }

    /// Sizes the window to the items and moves every icon visual to its slot.
    fn relayout(&self) {
        let s = self.scale();
        let items = self.items.borrow();
        let (w, h) = (width(items.len()) * s, height() * s);
        let strip = self.strip.get();
        let x = (strip.left + strip.right) / 2 - (w / 2.0) as i32;
        let y = strip.bottom - theme::px(theme::DOCK_MARGIN, self.dpi.get()) - h.round() as i32;
        unsafe { _ = SetWindowPos(self.hwnd.get(), Some(HWND_TOPMOST), x, y, w.round() as i32, h.round() as i32, SWP_NOACTIVATE) };

        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        _ = c.shape.SetSize(Vector2 { X: w, Y: h });
        _ = c.shape.SetCornerRadius(Vector2 { X: theme::DOCK_RADIUS * s, Y: theme::DOCK_RADIUS * s });
        let icon = theme::DOCK_ICON * s;
        let dot = theme::DOCK_DOT * s;
        for (i, item) in items.iter().enumerate() {
            let left = (item_x(i) * s).round();
            _ = item.icon.SetOffset(Vector3 { X: left, Y: (theme::DOCK_PAD * s).round(), Z: 0.0 });
            _ = item.icon.SetSize(Vector2 { X: icon, Y: icon });
            let dot_y = (theme::DOCK_PAD + theme::DOCK_ICON + (theme::DOCK_PAD_BOTTOM - theme::DOCK_DOT) / 2.0) * s;
            _ = item.dot.SetOffset(Vector3 { X: (left + (icon - dot) / 2.0).round(), Y: dot_y.round(), Z: 0.0 });
            _ = item.dot.SetSize(Vector2 { X: dot, Y: dot });
            _ = item.dot.SetIsVisible(!item.windows.is_empty());
        }
    }

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

    /// Uses the cached icon for `source`, or asks the worker for it.
    fn set_icon(&self, visual: &SpriteVisual, source: &str) {
        if let Some(brush) = self.brushes.borrow().get(source) {
            _ = visual.SetBrush(brush);
            return;
        }
        if self.requested.borrow_mut().insert(source.to_string())
            && let Some(loader) = self.loader.borrow().as_ref()
        {
            let size = (theme::DOCK_ICON * theme::DOCK_MAGNIFY * self.scale()).round() as u32;
            loader.request(source, size);
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
        if self.tracks(hwnd) || !winlist::qualifies(hwnd) {
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
        let mut removed = None;
        {
            let mut items = self.items.borrow_mut();
            for item in items.iter_mut() {
                item.windows.retain(|w| *w != hwnd);
            }
            if let Some(i) = items.iter().position(|i| i.pin.is_none() && i.windows.is_empty()) {
                removed = Some(items.remove(i));
            }
        }
        if let (Some(item), Some(c)) = (removed, self.comp.borrow().as_ref()) {
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.icon));
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.dot));
        }
        self.relayout();
    }

    fn on_shell(&self, event: ShellEvent) {
        match event {
            ShellEvent::Created(h) | ShellEvent::Activated(h) | ShellEvent::Redraw(h) => self.add_window(h),
            ShellEvent::Destroyed(h) => self.remove_window(h),
            ShellEvent::Replaced(h) => {
                self.remove_window(h);
            }
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

    /// macOS click: start the app, bring its window forward, minimise it when it
    /// is already in front, or cycle through several windows.
    fn click(&self, x_px: i32, new_instance: bool) {
        let n = self.items.borrow().len();
        let Some(i) = hit(n, x_px as f32 / self.scale()) else { return };
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
        match msg {
            m if m == self.shellhook && m != 0 => self.on_shell(winlist::decode(wp.0, lp.0)),
            WM_ICON => self.on_icon(unsafe { icons::take(lp) }),
            WM_RECHECK => self.recheck(HWND(wp.0 as *mut _)),
            WM_LBUTTONUP => self.click((lp.0 & 0xFFFF) as i16 as i32, false),
            WM_MBUTTONUP => self.click((lp.0 & 0xFFFF) as i16 as i32, true),
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
                if !self.hidden {
                    appbar::remove(hwnd);
                }
                unsafe { _ = DeregisterShellHookWindow(hwnd) };
            }
            m if m == self.taskbar_created && !self.hidden => {
                // Explorer restarted: new taskbar to hide, every AppBar forgotten.
                crate::shelltray::hide();
                appbar::register(hwnd, WM_APPBAR);
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

    #[test]
    fn width_grows_by_icon_and_gap() {
        let one = width(1);
        assert_eq!(one, 2.0 * theme::DOCK_PAD + theme::DOCK_ICON);
        assert_eq!(width(3) - width(2), theme::DOCK_ICON + theme::DOCK_GAP);
        assert_eq!(width(0), one, "an empty dock keeps one slot");
    }

    #[test]
    fn hit_splits_gaps_between_neighbours() {
        let first = item_x(0);
        assert_eq!(hit(3, first + 1.0), Some(0));
        assert_eq!(hit(3, item_x(1) - theme::DOCK_GAP / 2.0 + 0.1), Some(1));
        assert_eq!(hit(3, item_x(1) - theme::DOCK_GAP / 2.0 - 0.1), Some(0));
        assert_eq!(hit(3, item_x(2) + theme::DOCK_ICON), Some(2));
        assert_eq!(hit(3, width(3) + 5.0), None);
        assert_eq!(hit(3, -1.0), None);
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
