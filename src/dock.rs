//! Bottom dock: pinned and running apps, one composition visual per icon so
//! effects only move visuals, never redraw. Icon size is adjustable and the
//! dock can hide itself, like the Ubuntu dock.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use windows::Foundation::TimeSpan;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionDrawingSurface, CompositionRoundedRectangleGeometry, CompositionSurfaceBrush,
    AnimationIterationBehavior, Compositor, ContainerVisual, ImplicitAnimationCollection, SpriteVisual,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
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
use crate::settings::{DockSide, self, Settings};
use crate::theme::{self, Palette};
use crate::window::{self, Backdrop, Handler};
use crate::winlist::{self, ShellEvent};
use crate::{appbar, log, magnify, menu, popup, preview};

pub const CLASS: PCWSTR = w!("TaskbarDock");
const WM_APPBAR: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 4;
/// A window's cloaked state changed (Store apps, virtual desktops): re-check it.
const WM_RECHECK: u32 = WM_APP + 5;
/// `--settings` from a second launch asks the running instance to open settings.
pub const WM_OPEN_SETTINGS: u32 = WM_APP + 6;
/// Keep the dock out while settings change it: wparam 1 hold, 0 release, 2 peek.
pub const WM_HOLD: u32 = WM_APP + 7;
/// A full-screen app came (wparam 1) or left (0): the dock steps aside.
pub const WM_FULLSCREEN: u32 = WM_APP + 8;
/// Lives in UI::Controls in windows-rs; not worth the feature for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HIDE: usize = 1;
const TIMER_REGION: usize = 2;
/// After magnified icons settle back, shrink the window region to the dock.
const TIMER_REST: usize = 3;
/// Entering animation done: icons now follow the pointer directly.
const TIMER_FOLLOW: usize = 4;
/// Hover this long on an icon before its preview opens.
const TIMER_PREVIEW: usize = 5;
/// Release a short "peek" hold (settings changed from the keyboard).
const TIMER_PEEK: usize = 6;
/// Launch bounces are over: shrink the window region again.
const TIMER_BOUNCE: usize = 7;
const PREVIEW_DELAY_MS: u32 = 350;
/// One bounce, up and down, and its height as a share of the icon (macOS).
const BOUNCE_MS: u32 = 640;
const BOUNCE_UP: f32 = 0.5;
const LAUNCH_BOUNCES: u32 = 3;
const PEEK_MS: u32 = 1200;
/// Icons follow the pointer through a short animation, which also smooths the return.
const FOLLOW_MS: u32 = 120;
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
    /// First item right of the separator (folders, Trash), as on macOS.
    pub sep: Option<usize>,
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
    /// Room the separator line takes between the two sections.
    pub fn sep_w(self) -> f32 {
        self.gap() + 1.0
    }
    /// Whether item `i` of `n` starts the right-hand section.
    pub fn sep_before(self, i: usize, n: usize) -> bool {
        self.sep == Some(i) && i > 0 && i < n
    }
    pub fn width(self, n: usize) -> f32 {
        let has_sep = self.sep.is_some_and(|s| s > 0 && s < n);
        let n = n.max(1) as f32;
        2.0 * self.pad() + n * self.icon + (n - 1.0) * self.gap() + if has_sep { self.sep_w() } else { 0.0 }
    }
    pub fn height(self) -> f32 {
        self.pad() + self.icon + self.pad_bottom()
    }
    pub fn item_x(self, i: usize) -> f32 {
        let past_sep = self.sep.is_some_and(|s| s > 0 && i >= s);
        self.pad() + i as f32 * (self.icon + self.gap()) + if past_sep { self.sep_w() } else { 0.0 }
    }
}

/// Window position of a box laid out as on a bottom dock: `a` along the dock,
/// `c` from the far side towards the screen edge, `sc` thick across; `lh` is
/// the window's thickness across the dock. Side docks turn this a quarter.
pub fn turn(side: DockSide, lh: f32, a: f32, c: f32, sc: f32) -> (f32, f32) {
    match side {
        DockSide::Bottom => (a, c),
        DockSide::Left => (lh - c - sc, a),
        DockSide::Right => (c, a),
    }
}

/// Window size of a box `sa` along and `sc` across the dock.
pub fn self_dims(side: DockSide, sa: f32, sc: f32) -> (f32, f32) {
    match side {
        DockSide::Bottom => (sa, sc),
        _ => (sc, sa),
    }
}

// ---- model ----

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    App,
    /// "Show applications" button, first in the dock; opens Launchpad.
    Launchpad,
    /// Pinned folder right of the separator; opens as a stack.
    Folder,
    Trash,
}

const TRASH: &str = r"shell:RecycleBinFolder";

struct Item {
    kind: Kind,
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
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let dir = std::path::Path::new(&appdata)
        .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let mut links: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")))
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
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
        .filter_map(|p| {
            Some(Entry {
                identity: appid::source_identity(&p)?,
                pin: Some(p.clone()),
                windows: Vec::new(),
                source: p,
            })
        })
        .collect();
    for hwnd in winlist::enumerate()
        .into_iter()
        .rev()
        .filter(|h| !winlist::is_own(*h))
    {
        let Some(identity) = appid::window_identity(hwnd) else {
            continue;
        };
        match out.iter_mut().find(|e| e.identity.same_app(&identity)) {
            Some(e) => e.windows.push(hwnd),
            None => {
                if let Some(source) = source_for(&identity) {
                    out.push(Entry {
                        identity,
                        pin: None,
                        windows: vec![hwnd],
                        source,
                    });
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
    /// Blurred background; widens with magnification.
    frame: ContainerVisual,
    backdrop: Backdrop,
    /// Implicit Offset/Scale/Size animations shared by icons, dots and the frame.
    follow: ImplicitAnimationCollection,
    /// Empty collection: values apply at once while the pointer moves.
    still: ImplicitAnimationCollection,
    shape: CompositionRoundedRectangleGeometry,
    items: ContainerVisual,
    dot_surface: CompositionDrawingSurface,
    dot_brush: CompositionSurfaceBrush,
    /// Thin line between apps and folders/Trash.
    sep_line: SpriteVisual,
    sep_brush: windows::UI::Composition::CompositionColorBrush,
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
    /// Pointer x over the dock (DIPs from the resting dock's left edge) while magnifying.
    pointer: Cell<Option<f32>>,
    /// Animate the next layout change (enter, leave, items); off while following the pointer.
    smooth: Cell<bool>,
    /// Item under the pointer (for previews).
    hovered: Cell<Option<usize>>,
    /// Settings window keeps the dock out.
    held: Cell<bool>,
    trash_full: Cell<bool>,
    tracking: Cell<bool>,
    registered: Cell<bool>,
    items: RefCell<Vec<Item>>,
    comp: RefCell<Option<Comp>>,
    brushes: RefCell<HashMap<String, CompositionSurfaceBrush>>,
    requested: RefCell<HashSet<String>>,
    loader: RefCell<Option<Loader>>,
    placing: Cell<bool>,
    /// Bouncing icons by source: true = asks for attention (until activated),
    /// false = launching (until its first window or three bounces).
    bouncing: RefCell<HashMap<String, bool>>,
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
            pointer: Cell::new(None),
            smooth: Cell::new(true),
            hovered: Cell::new(None),
            held: Cell::new(false),
            trash_full: Cell::new(false),
            tracking: Cell::new(false),
            registered: Cell::new(false),
            items: RefCell::new(Vec::new()),
            comp: RefCell::new(None),
            brushes: RefCell::new(HashMap::new()),
            requested: RefCell::new(HashSet::new()),
            loader: RefCell::new(None),
            placing: Cell::new(false),
            bouncing: RefCell::new(HashMap::new()),
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
        dock.add_launchpad();
        dock.sync_stacks();
        unsafe {
            _ = RegisterShellHookWindow(hwnd);
            SetWinEventHook(
                EVENT_OBJECT_CLOAKED,
                EVENT_OBJECT_UNCLOAKED,
                None,
                Some(on_cloak),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
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
        Geometry {
            sep: self.items.borrow().iter().position(|i| matches!(i.kind, Kind::Folder | Kind::Trash)),
            icon: settings::get().dock_icon,
        }
    }

    fn autohide(&self) -> bool {
        settings::get().dock_autohide
    }

    fn side(&self) -> DockSide {
        settings::get().dock_side
    }

    // The dock is laid out as if it sat at the bottom: `a` runs along it, `c`
    // across it from the far side towards the screen edge (px). Side docks
    // turn that a quarter, so icons still grow away from the edge.

    /// Window px offset of a box at (`a`, `c`), `sc` thick across the dock.
    fn at(&self, a: f32, c: f32, sc: f32) -> Vector3 {
        let (x, y) = turn(self.side(), self.size.get().1 as f32, a, c, sc);
        Vector3 { X: x, Y: y, Z: 0.0 }
    }

    fn dims(&self, sa: f32, sc: f32) -> Vector2 {
        let (x, y) = self_dims(self.side(), sa, sc);
        Vector2 { X: x, Y: y }
    }

    /// A point inside a box `sc` thick across the dock, for centre points.
    fn inside(&self, pa: f32, pc: f32, sc: f32) -> Vector3 {
        let (x, y) = match self.side() {
            DockSide::Bottom => (pa, pc),
            DockSide::Left => (sc - pc, pa),
            DockSide::Right => (pc, pa),
        };
        Vector3 { X: x, Y: y, Z: 0.0 }
    }

    /// Popup anchor for a dock point (`a`, `c`) in DIPs: beside the dock, on its far side.
    fn anchor(&self, a: f32, c: f32) -> (i32, popup::Place) {
        let s = self.scale();
        let mut rc = RECT::default();
        unsafe { _ = GetWindowRect(self.hwnd.get(), &mut rc) };
        let (a, c) = ((a * s).round() as i32, (c * s).round() as i32);
        let lh = self.size.get().1;
        match self.side() {
            DockSide::Bottom => (rc.left + a, popup::Place::Above(rc.top + c)),
            DockSide::Left => (rc.top + a, popup::Place::RightOf(rc.left + lh - c)),
            DockSide::Right => (rc.top + a, popup::Place::LeftOf(rc.left + c)),
        }
    }

    fn edge(&self) -> appbar::Edge {
        match self.side() {
            DockSide::Bottom => appbar::Edge::Bottom,
            DockSide::Left => appbar::Edge::Left,
            DockSide::Right => appbar::Edge::Right,
        }
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
        let ease_out = c.CreateCubicBezierEasingFunction(
            Vector2 { X: 0.2, Y: 0.0 },
            Vector2 { X: 0.0, Y: 1.0 },
        )?;
        anim.InsertExpressionKeyFrameWithEasingFunction(
            1.0,
            &HSTRING::from("this.FinalValue"),
            &ease_out,
        )?;
        anim.SetDuration(TimeSpan {
            Duration: SLIDE_MS as i64 * 10_000,
        })?;
        let implicit = c.CreateImplicitAnimationCollection()?;
        implicit.Insert(&HSTRING::from("Offset"), &anim)?;
        slide.SetImplicitAnimations(&implicit)?;

        let follow = follow_animations(c)?;
        // Rounded frame clips the blur and tint; icons sit above it unclipped (they grow out of it).
        let frame = c.CreateContainerVisual()?;
        let shape = c.CreateRoundedRectangleGeometry()?;
        frame.SetClip(&c.CreateGeometricClipWithGeometry(&shape)?)?;
        frame.SetImplicitAnimations(&follow)?;
        shape.SetImplicitAnimations(&follow)?;
        let backdrop = Backdrop::new(c, self.hwnd.get())?;
        frame.Children()?.InsertAtTop(&backdrop.blur)?;
        frame.Children()?.InsertAtTop(&backdrop.tint_visual)?;

        let items = c.CreateContainerVisual()?;
        let sep_brush = c.CreateColorBrush()?;
        let sep_line = c.CreateSpriteVisual()?;
        sep_line.SetBrush(&sep_brush)?;
        sep_line.SetImplicitAnimations(&follow)?;
        items.Children()?.InsertAtTop(&sep_line)?;
        let dot_surface = self.gfx.surface(1, 1)?;
        let dot_brush = c.CreateSurfaceBrushWithSurface(&dot_surface)?;

        slide.Children()?.InsertAtTop(&frame)?;
        slide.Children()?.InsertAtTop(&items)?;
        root.Children()?.InsertAtTop(&slide)?;
        target.SetRoot(&root)?;
        backdrop.apply(self.palette.get().bar);
        let still = c.CreateImplicitAnimationCollection()?;
        *self.comp.borrow_mut() = Some(Comp {
            _target: target,
            slide,
            frame,
            backdrop,
            follow,
            still,
            sep_line,
            sep_brush,
            shape,
            items,
            dot_surface,
            dot_brush,
        });
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

    /// Screen strip the resting dock needs (what the AppBar reserves).
    fn thickness_px(&self) -> i32 {
        theme::px(
            self.geometry().height() + theme::DOCK_MARGIN,
            self.dpi.get(),
        )
    }

    /// Room around the resting dock for magnified icons (DIPs): (each side, above).
    fn headroom(&self) -> (f32, f32) {
        let g = self.geometry();
        let grow = g.icon * (settings::get().dock_magnify - 1.0);
        // Above: room for magnified icons and for bouncing ones.
        (grow * (magnify::RADIUS + 0.5), grow.max(g.icon * BOUNCE_UP))
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
            appbar::set_edge(self.hwnd.get(), self.edge(), monitor, thickness)
        } else {
            appbar::edge_rect(monitor, self.edge(), thickness)
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
        let n = self.items.borrow().len();
        let (side, above) = self.headroom();
        let w = ((g.width(n) + 2.0 * side) * s).round() as i32;
        let h = self.thickness_px() + (above * s).round() as i32;
        let strip = self.strip.get();
        let (mid_x, mid_y) = ((strip.left + strip.right) / 2, (strip.top + strip.bottom) / 2);
        let (x, y, ww, wh) = match self.side() {
            DockSide::Bottom => (mid_x - w / 2, strip.bottom - h, w, h),
            DockSide::Left => (strip.left, mid_y - w / 2, h, w),
            DockSide::Right => (strip.right - h, mid_y - w / 2, h, w),
        };
        unsafe {
            _ = SetWindowPos(
                self.hwnd.get(),
                Some(HWND_TOPMOST),
                x,
                y,
                ww,
                wh,
                SWP_NOACTIVATE,
            )
        };
        // Logical size: along the dock, then across it.
        self.size.set((w, h));
        if let Some(c) = self.comp.borrow().as_ref() {
            // Children carry the headroom themselves (see `at`).
            _ = c.items.SetOffset(Vector3::default());
            _ = c.shape.SetCornerRadius(Vector2 {
                X: g.radius() * s,
                Y: g.radius() * s,
            });
        }
        self.position_items();
        self.update_region();
    }

    /// Moves icons, dots and the background to the current magnification.
    /// Only composition properties change; DWM animates them.
    fn position_items(&self) {
        let s = self.scale();
        let g = self.geometry();
        let items = self.items.borrow();
        let l = magnify::layout(
            g,
            items.len(),
            self.pointer.get(),
            settings::get().dock_magnify,
        );
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let (side, above) = self.headroom();
        let (side_px, above_px) = ((side * s).round(), (above * s).round());
        let (fa, fc) = (((l.right - l.left) * s).round(), (g.height() * s).round());
        let frame = self.dims(fa, fc);
        _ = c.frame.SetOffset(self.at(((side + l.left) * s).round(), above_px, fc));
        _ = c.frame.SetSize(frame);
        // Separator: centred in its room before the first folder/Trash item.
        let line_h = (g.icon * 0.7 * s).round();
        match g.sep.filter(|i| g.sep_before(*i, items.len())) {
            Some(i) => {
                let x = ((l.items[i].0 - g.sep_w() / 2.0 - g.gap() / 2.0) * s).round();
                _ = c.sep_line.SetIsVisible(true);
                let line_w = s.round().max(1.0);
                _ = c.sep_line.SetSize(self.dims(line_w, line_h));
                _ = c.sep_line.SetOffset(self.at(side_px + x, above_px + ((g.pad() + g.icon) * s - line_h).round(), line_h));
                _ = c.sep_brush.SetColor(theme::Rgba(self.palette.get().text.0, self.palette.get().text.1, self.palette.get().text.2, 70).ui());
            }
            None => _ = c.sep_line.SetIsVisible(false),
        }
        _ = c.shape.SetSize(frame);
        // Even pixel sizes so icon and dot share one exact centre line.
        let even = |v: f32| ((v / 2.0).round() * 2.0).max(2.0);
        let icon = even(g.icon * s);
        let dot = even(theme::DOCK_DOT * s);
        let dot_y = ((g.pad() + g.icon + (g.pad_bottom() - theme::DOCK_DOT) / 2.0) * s).round();
        let indicators = settings::get().dock_indicators;
        let anims = if self.smooth.get() {
            &c.follow
        } else {
            &c.still
        };
        _ = c.frame.SetImplicitAnimations(anims);
        _ = c.shape.SetImplicitAnimations(anims);
        for (item, (left, k)) in items.iter().zip(&l.items) {
            let centre = ((left + g.icon * k / 2.0) * s).round();
            _ = item.icon.SetImplicitAnimations(anims);
            _ = item.dot.SetImplicitAnimations(anims);
            // Scale grows up from the bottom centre, like the macOS dock.
            _ = item.icon.SetSize(Vector2 { X: icon, Y: icon });
            _ = item.icon.SetCenterPoint(self.inside(icon / 2.0, icon, icon));
            _ = item.icon.SetScale(Vector3 {
                X: *k,
                Y: *k,
                Z: 1.0,
            });
            _ = item.icon.SetOffset(self.at(side_px + centre - icon / 2.0, above_px + (g.pad() * s).round(), icon));
            _ = item.dot.SetSize(Vector2 { X: dot, Y: dot });
            _ = item.dot.SetOffset(self.at(side_px + centre - dot / 2.0, above_px + dot_y, dot));
            _ = item
                .dot
                .SetIsVisible(indicators && !item.windows.is_empty());
        }
    }

    /// Window region: the trigger strip when hidden, the whole window while
    /// magnifying (icons grow above the dock), otherwise just the resting dock,
    /// so clicks beside and above it reach the apps underneath.
    fn update_region(&self) {
        let (w, h) = self.size.get();
        let s = self.scale();
        let (side, _) = self.headroom();
        let left = (side * s).round() as i32;
        let right = w - left;
        // Logical (along, across) rect to window px.
        let rgn = |l: i32, t: i32, r: i32, b: i32| unsafe {
            let (x0, y0) = turn(self.side(), h as f32, l as f32, t as f32, (b - t) as f32);
            let size = self_dims(self.side(), (r - l) as f32, (b - t) as f32);
            Some(CreateRectRgn(x0 as i32, y0 as i32, x0 as i32 + size.0 as i32, y0 as i32 + size.1 as i32))
        };
        unsafe {
            if !self.shown.get() {
                SetWindowRgn(self.hwnd.get(), rgn(left, h - TRIGGER_PX, right, h), true);
            } else if self.pointer.get().is_some() || !self.bouncing.borrow().is_empty() {
                SetWindowRgn(self.hwnd.get(), None, true);
            } else {
                SetWindowRgn(self.hwnd.get(), rgn(left, h - self.thickness_px(), right, h), true);
            }
        }
    }

    /// Pointer x in DIPs from the resting dock's left edge.
    fn dock_x(&self, x_px: i32) -> f32 {
        x_px as f32 / self.scale() - self.headroom().0
    }

    // ---- auto-hide ----

    fn slide_to(&self, out: bool) {
        let d = if out { self.size.get().1 as f32 } else { 0.0 };
        let (x, y) = match self.side() {
            DockSide::Bottom => (0.0, d),
            DockSide::Left => (-d, 0.0),
            DockSide::Right => (d, 0.0),
        };
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.slide.SetOffset(Vector3 { X: x, Y: y, Z: 0.0 });
        }
    }

    fn slide_out(&self) {
        self.shown.set(false);
        self.slide_to(true);
        self.pointer.set(None);
        self.position_items();
        // Clip the window to the trigger strip once the slide has finished, so apps get the clicks.
        unsafe { SetTimer(Some(self.hwnd.get()), TIMER_REGION, SLIDE_MS + 30, None) };
    }

    fn slide_in(&self) {
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_REGION) };
        self.shown.set(true);
        self.update_region();
        self.slide_to(false);
    }

    fn on_mouse_move(&self, x_px: i32) {
        if !self.tracking.replace(true) {
            let mut t = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd.get(),
                dwHoverTime: 0,
            };
            unsafe { _ = TrackMouseEvent(&mut t) };
        }
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_HIDE) };
        if self.autohide() && !self.shown.get() {
            self.slide_in();
            return;
        }
        self.track_preview(x_px);
        if settings::get().dock_magnify <= 1.0 {
            return;
        }
        // Magnify while the pointer is over the dock's current extent.
        let x = self.dock_x(x_px);
        let g = self.geometry();
        let l = magnify::layout(
            g,
            self.items.borrow().len(),
            self.pointer.get(),
            settings::get().dock_magnify,
        );
        if x < l.left || x >= l.right {
            return self.rest();
        }
        let first = self.pointer.replace(Some(x)).is_none();
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_REST) };
        if first {
            self.refresh_trash();
            // Grow in with the animation, then follow the pointer directly (no restarts, no lag).
            self.smooth.set(true);
            unsafe { SetTimer(Some(self.hwnd.get()), TIMER_FOLLOW, FOLLOW_MS, None) };
            self.update_region();
        }
        self.position_items();
    }

    /// Popup anchor at item `i`'s centre, just beyond its (magnified) icon.
    fn icon_anchor(&self, i: usize) -> (i32, popup::Place) {
        let g = self.geometry();
        let l = magnify::layout(g, self.items.borrow().len(), self.pointer.get(), settings::get().dock_magnify);
        let (left, k) = l.items.get(i).copied().unwrap_or((0.0, 1.0));
        let (side, above) = self.headroom();
        self.anchor(side + left + g.icon * k / 2.0, above + g.pad() + g.icon * (1.0 - k) - 8.0)
    }

    // ---- hover previews ----

    fn track_preview(&self, x_px: i32) {
        let item = self.item_at(x_px);
        if item == self.hovered.get() {
            return;
        }
        self.hovered.set(item);
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_PREVIEW) };
        match item {
            // Already showing one: follow the pointer at once, like the Windows taskbar.
            Some(i) if preview::visible() => self.show_preview(i),
            Some(_) => unsafe {
                _ = SetTimer(Some(self.hwnd.get()), TIMER_PREVIEW, PREVIEW_DELAY_MS, None)
            },
            None => preview::hide_soon(),
        }
    }

    fn show_preview(&self, i: usize) {
        let (windows, source) = {
            let items = self.items.borrow();
            let Some(item) = items.get(i) else { return };
            (
                item.windows.clone(),
                item.pin.clone().unwrap_or_else(|| item.source.clone()),
            )
        };
        let name = if windows.is_empty() {
            appid::source_name(&source)
        } else {
            String::new()
        };
        let (anchor, place) = self.icon_anchor(i);
        preview::show(&windows, &name, anchor, place);
    }

    /// Back to resting size; the region shrinks once the icons have settled.
    fn rest(&self) {
        if self.pointer.take().is_some() {
            self.smooth.set(true);
            self.position_items();
            unsafe { SetTimer(Some(self.hwnd.get()), TIMER_REST, FOLLOW_MS + 60, None) };
        }
    }

    /// Settings window: keep the dock out so size changes are visible.
    fn hold(&self, how: usize) {
        let hwnd = self.hwnd.get();
        unsafe {
            _ = KillTimer(Some(hwnd), TIMER_PEEK);
            match how {
                // Release waits for an open menu to close.
                0 if menu::visible() => _ = SetTimer(Some(hwnd), TIMER_PEEK, 300, None),
                0 => {
                    self.held.set(false);
                    if self.autohide() && !popup::pointer_in(hwnd) {
                        SetTimer(Some(hwnd), TIMER_HIDE, HIDE_DELAY_MS, None);
                    }
                }
                _ => {
                    self.held.set(true);
                    _ = KillTimer(Some(hwnd), TIMER_HIDE);
                    if !self.shown.get() {
                        self.slide_in();
                    }
                    if how == 2 {
                        SetTimer(Some(hwnd), TIMER_PEEK, PEEK_MS, None);
                    }
                }
            }
        }
    }

    fn on_mouse_leave(&self) {
        self.tracking.set(false);
        self.hovered.set(None);
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_PREVIEW) };
        preview::hide_soon();
        self.rest();
        if self.autohide() && self.shown.get() && !self.held.get() {
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
                dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 { X: r, Y: r },
                        radiusX: r,
                        radiusY: r,
                    },
                    &brush,
                );
                Ok(())
            })
        });
        if let Err(e) = r {
            log::error(&format!("dock dot: {e}"));
        }
    }

    fn add_item(
        &self,
        identity: AppIdentity,
        pin: Option<String>,
        source: String,
    ) -> Option<usize> {
        self.add_kind(Kind::App, identity, pin, source)
    }

    fn add_kind(
        &self,
        kind: Kind,
        identity: AppIdentity,
        pin: Option<String>,
        source: String,
    ) -> Option<usize> {
        let comp = self.comp.borrow();
        let c = comp.as_ref()?;
        let compositor = &self.gfx.compositor;
        let icon = compositor.CreateSpriteVisual().ok()?;
        let dot = compositor.CreateSpriteVisual().ok()?;
        _ = icon.SetImplicitAnimations(&c.follow);
        _ = dot.SetImplicitAnimations(&c.follow);
        _ = dot.SetBrush(&c.dot_brush);
        _ = c.items.Children().and_then(|ch| ch.InsertAtTop(&icon));
        _ = c.items.Children().and_then(|ch| ch.InsertAtTop(&dot));
        drop(comp);
        self.set_icon(&icon, &source);
        let mut items = self.items.borrow_mut();
        // Apps stay left of the separator; folders before Trash; Trash last.
        let at = match kind {
            Kind::Launchpad => Some(0),
            Kind::App => items.iter().position(|i| matches!(i.kind, Kind::Folder | Kind::Trash)),
            Kind::Folder => items.iter().position(|i| i.kind == Kind::Trash),
            Kind::Trash => None,
        }
        .unwrap_or(items.len());
        items.insert(at, Item {
            kind,
            identity,
            pin,
            windows: Vec::new(),
            source,
            icon,
            dot,
        });
        Some(at)
    }

    /// The Launchpad button with its own drawn icon (no shell icon to load).
    fn add_launchpad(&self) {
        let none = AppIdentity { aumid: None, exe: None };
        let Some(i) = self.add_kind(Kind::Launchpad, none, Some("Launchpad".into()), "Launchpad".into()) else { return };
        let px = (settings::DOCK_ICON_MAX * settings::MAGNIFY_MAX * self.scale()).round() as u32;
        let brush = self.gfx.surface(px, px).and_then(|surface| {
            self.gfx.draw(&surface, 1.0, |dc| crate::launchpad::draw_button(dc, px as f32))?;
            self.gfx.compositor.CreateSurfaceBrushWithSurface(&surface)
        });
        match brush {
            Ok(b) => {
                _ = self.items.borrow()[i].icon.SetBrush(&b);
                self.brushes.borrow_mut().insert("Launchpad".into(), b);
            }
            Err(e) => log::error(&format!("launchpad button: {e}")),
        }
    }

    /// Folders from the settings, then Trash, right of the separator.
    fn sync_stacks(&self) {
        loop {
            // Own statement: a `while let` scrutinee keeps the borrow for the
            // whole body, and remove_item needs items mutably.
            let found = self.items.borrow().iter().position(|i| matches!(i.kind, Kind::Folder | Kind::Trash));
            let Some(i) = found else { break };
            self.remove_item(i);
        }
        let none = || AppIdentity { aumid: None, exe: None };
        for folder in settings::folders(&settings::get()) {
            self.add_kind(Kind::Folder, none(), Some(folder.clone()), folder);
        }
        self.add_kind(Kind::Trash, none(), Some(TRASH.into()), TRASH.into());
        self.trash_full.set(trash_full());
        self.relayout();
    }

    /// Trash icon follows its state (empty/full); checked when the pointer arrives.
    fn refresh_trash(&self) {
        let full = trash_full();
        if self.trash_full.replace(full) == full {
            return;
        }
        self.brushes.borrow_mut().remove(TRASH);
        self.requested.borrow_mut().remove(TRASH);
        if let Some(icon) = self.items.borrow().iter().find(|i| i.kind == Kind::Trash).map(|i| i.icon.clone()) {
            self.set_icon(&icon, TRASH);
        }
    }

    fn remove_item(&self, index: usize) {
        let item = self.items.borrow_mut().remove(index);
        self.bouncing.borrow_mut().remove(&item.source);
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.icon));
            _ = c.items.Children().and_then(|ch| ch.Remove(&item.dot));
        }
    }

    /// Uses the cached icon for `source`, or asks the worker for it. Icons are
    /// extracted once at the largest size and magnification, then scaled down.
    fn set_icon(&self, visual: &SpriteVisual, source: &str) {
        if source == "Launchpad" && !self.brushes.borrow().contains_key(source) {
            return; // drawn by add_launchpad
        }
        if let Some(brush) = self.brushes.borrow().get(source) {
            _ = visual.SetBrush(brush);
            return;
        }
        if self.requested.borrow_mut().insert(source.to_string())
            && let Some(loader) = self.loader.borrow().as_ref()
        {
            loader.request(
                source,
                (settings::DOCK_ICON_MAX * settings::MAGNIFY_MAX * self.scale()).round() as u32,
            );
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
        for item in self
            .items
            .borrow()
            .iter()
            .filter(|i| i.source == icon.source)
        {
            _ = item.icon.SetBrush(&brush);
        }
        self.brushes.borrow_mut().insert(icon.source.clone(), brush);
    }

    fn icon_brush(&self, icon: &icons::Icon) -> Result<CompositionSurfaceBrush> {
        self.gfx.icon_brush(icon)
    }

    fn tracks(&self, hwnd: HWND) -> bool {
        self.items
            .borrow()
            .iter()
            .any(|i| i.windows.contains(&hwnd))
    }

    fn add_window(&self, hwnd: HWND) {
        if self.tracks(hwnd) || !winlist::qualifies(hwnd) || winlist::is_own(hwnd) {
            return;
        }
        let Some(identity) = appid::window_identity(hwnd) else {
            return;
        };
        let found = self
            .items
            .borrow()
            .iter()
            .position(|i| i.identity.same_app(&identity));
        let index = match found {
            Some(i) => i,
            None => {
                let Some(source) = source_for(&identity) else {
                    return;
                };
                match self.add_item(identity, None, source) {
                    Some(i) => i,
                    None => return,
                }
            }
        };
        self.items.borrow_mut()[index].windows.push(hwnd);
        let source = self.items.borrow()[index].source.clone();
        if self.bouncing.borrow().get(&source) == Some(&false) {
            self.stop_bounce(&source); // launched: its window is here
        }
        self.relayout();
    }

    // ---- bouncing (launch and attention) ----

    /// Bounces item `source` up and down: three times for a launch, until
    /// stopped for attention. Runs on the compositor; the icon's Translation
    /// moves, so magnification (Offset/Scale) keeps working meanwhile.
    fn bounce(&self, source: &str, attention: bool) {
        if self.hidden || self.bouncing.borrow().contains_key(source) {
            return;
        }
        let items = self.items.borrow();
        let Some(item) = items.iter().find(|i| i.source == source) else { return };
        let c = &self.gfx.compositor;
        let up = -(settings::get().dock_icon * BOUNCE_UP * self.scale()).round();
        let run = || -> Result<()> {
            item.icon.Properties()?.InsertVector3(&HSTRING::from("Translation"), Vector3::default())?;
            let rise = c.CreateCubicBezierEasingFunction(Vector2 { X: 0.3, Y: 0.0 }, Vector2 { X: 0.4, Y: 1.0 })?;
            let fall = c.CreateCubicBezierEasingFunction(Vector2 { X: 0.6, Y: 0.0 }, Vector2 { X: 0.8, Y: 0.6 })?;
            let a = c.CreateVector3KeyFrameAnimation()?;
            a.InsertKeyFrameWithEasingFunction(0.45, Vector3 { X: 0.0, Y: up, Z: 0.0 }, &rise)?;
            a.InsertKeyFrameWithEasingFunction(1.0, Vector3::default(), &fall)?;
            a.SetDuration(TimeSpan { Duration: BOUNCE_MS as i64 * 10_000 })?;
            if attention {
                a.SetIterationBehavior(AnimationIterationBehavior::Forever)?;
            } else {
                a.SetIterationBehavior(AnimationIterationBehavior::Count)?;
                a.SetIterationCount(LAUNCH_BOUNCES as i32)?;
            }
            item.icon.StartAnimation(&HSTRING::from("Translation"), &a)
        };
        if let Err(e) = run() {
            return log::error(&format!("bounce: {e}"));
        }
        drop(items);
        self.bouncing.borrow_mut().insert(source.to_string(), attention);
        self.update_region();
        if !attention {
            unsafe { SetTimer(Some(self.hwnd.get()), TIMER_BOUNCE, BOUNCE_MS * LAUNCH_BOUNCES + 50, None) };
        }
    }

    fn stop_bounce(&self, source: &str) {
        if self.bouncing.borrow_mut().remove(source).is_none() {
            return;
        }
        if let Some(item) = self.items.borrow().iter().find(|i| i.source == source) {
            _ = item.icon.StopAnimation(&HSTRING::from("Translation"));
            _ = item.icon.Properties().and_then(|p| p.InsertVector3(&HSTRING::from("Translation"), Vector3::default()));
        }
        self.update_region();
    }

    /// Launch bounces ran out: forget them (attention bounces keep going).
    fn end_launch_bounces(&self) {
        unsafe { _ = KillTimer(Some(self.hwnd.get()), TIMER_BOUNCE) };
        let done: Vec<String> = self.bouncing.borrow().iter().filter(|(_, a)| !**a).map(|(s, _)| s.clone()).collect();
        for s in done {
            self.stop_bounce(&s);
        }
    }

    fn source_of_window(&self, hwnd: HWND) -> Option<String> {
        self.items.borrow().iter().find(|i| i.windows.contains(&hwnd)).map(|i| i.source.clone())
    }

    fn remove_window(&self, hwnd: HWND) {
        for item in self.items.borrow_mut().iter_mut() {
            item.windows.retain(|w| *w != hwnd);
        }
        let empty = self
            .items
            .borrow()
            .iter()
            .position(|i| i.pin.is_none() && i.windows.is_empty());
        if let Some(i) = empty {
            self.remove_item(i);
        }
        self.relayout();
    }

    fn on_shell(&self, event: ShellEvent) {
        match event {
            ShellEvent::Activated(h) => {
                self.add_window(h);
                if let Some(s) = self.source_of_window(h) {
                    self.stop_bounce(&s);
                }
            }
            ShellEvent::Created(h) | ShellEvent::Redraw(h) => self.add_window(h),
            ShellEvent::Flash(h) => {
                self.add_window(h);
                if unsafe { GetForegroundWindow() } != h
                    && let Some(s) = self.source_of_window(h)
                {
                    self.bounce(&s, true);
                }
            }
            ShellEvent::Destroyed(h) | ShellEvent::Replaced(h) => {
                preview::forget(h);
                self.remove_window(h);
            }
            ShellEvent::Other => {}
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
        let g = self.geometry();
        let l = magnify::layout(
            g,
            self.items.borrow().len(),
            self.pointer.get(),
            settings::get().dock_magnify,
        );
        magnify::hit(g, &l, self.dock_x(x_px))
    }

    /// macOS click: start the app, bring its window forward, minimise it when it
    /// is already in front, or cycle through several windows.
    fn click(&self, x_px: i32, new_instance: bool) {
        let Some(i) = self.item_at(x_px) else { return };
        let (kind, source) = {
            let items = self.items.borrow();
            (items[i].kind, items[i].source.clone())
        };
        match kind {
            Kind::Folder => {
                let (anchor, place) = self.icon_anchor(i);
                return crate::stack::show(&source, anchor, place);
            }
            Kind::Trash => {
                unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(TRASH), None, None, SW_SHOWNORMAL) };
                return;
            }
            Kind::Launchpad => return crate::launchpad::toggle(),
            Kind::App => {}
        }
        let (windows, launch) = {
            let items = self.items.borrow();
            let item = &items[i];
            (
                item.windows.clone(),
                item.pin.clone().unwrap_or_else(|| item.source.clone()),
            )
        };
        self.stop_bounce(&source);
        if windows.is_empty() || new_instance {
            if windows.is_empty() {
                self.bounce(&source, false);
            }
            unsafe {
                ShellExecuteW(
                    None,
                    w!("open"),
                    &HSTRING::from(launch),
                    None,
                    None,
                    SW_SHOWNORMAL,
                )
            };
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

    /// Right click: Windows taskbar choices in a macOS style menu. On an app:
    /// its windows (checked = in front), new window, keep/remove, file location,
    /// close. On the dock itself: hiding, magnification, settings.
    fn context_menu(&self, x_px: i32) {
        const NEW: u32 = 1;
        const KEEP: u32 = 2;
        const LOCATION: u32 = 3;
        const CLOSE: u32 = 4;
        const AUTOHIDE: u32 = 5;
        const MAGNIFY: u32 = 6;
        const SETTINGS: u32 = 7;
        const WINDOW: u32 = 100;
        preview::hide();
        let index = self.item_at(x_px);
        if let Some(i) = index {
            let (kind, source) = {
                let items = self.items.borrow();
                (items[i].kind, items[i].source.clone())
            };
            if matches!(kind, Kind::Folder | Kind::Trash) {
                return self.stack_menu(i, kind, source);
            }
        }
        let mut entries = Vec::new();
        let (windows, launch, exe) = match index {
            Some(i) => {
                let items = self.items.borrow();
                let item = &items[i];
                let launch = item.pin.clone().unwrap_or_else(|| item.source.clone());
                (
                    item.windows.clone(),
                    Some((launch, item.pin.is_some())),
                    item.identity.exe.clone(),
                )
            }
            None => (Vec::new(), None, None),
        };
        if let Some((launch, pinned)) = &launch {
            let fg = unsafe { GetForegroundWindow() };
            for (n, w) in windows.iter().enumerate() {
                let mut buf = [0u16; 256];
                let len = unsafe { GetWindowTextW(*w, &mut buf) } as usize;
                let title = String::from_utf16_lossy(&buf[..len]);
                entries.push(menu::Entry::Item {
                    id: WINDOW + n as u32,
                    label: if title.is_empty() {
                        "Pencere".into()
                    } else {
                        title
                    },
                    checked: *w == fg,
                });
            }
            if !windows.is_empty() {
                entries.push(menu::Entry::Separator);
            }
            entries.push(menu::Entry::item(NEW, appid::source_name(launch)));
            entries.push(menu::Entry::item(
                KEEP,
                if *pinned {
                    "Dock'tan kaldır"
                } else {
                    "Dock'ta tut"
                },
            ));
            if exe.is_some() {
                entries.push(menu::Entry::item(LOCATION, "Dosya konumunu aç"));
            }
            if !windows.is_empty() {
                entries.push(menu::Entry::Separator);
                entries.push(menu::Entry::item(
                    CLOSE,
                    if windows.len() > 1 {
                        "Tüm pencereleri kapat"
                    } else {
                        "Pencereyi kapat"
                    },
                ));
            }
            // Always reachable: the empty dock edge is only a few pixels wide.
            entries.push(menu::Entry::Separator);
            entries.push(menu::Entry::item(SETTINGS, "Dock ayarları…"));
        } else {
            let st = settings::get();
            entries.push(menu::Entry::item(
                AUTOHIDE,
                if st.dock_autohide {
                    "Gizlemeyi kapat"
                } else {
                    "Gizlemeyi aç"
                },
            ));
            entries.push(menu::Entry::item(
                MAGNIFY,
                if st.dock_magnify > 1.0 {
                    "Büyütmeyi kapat"
                } else {
                    "Büyütmeyi aç"
                },
            ));
            entries.push(menu::Entry::Separator);
            entries.push(menu::Entry::item(SETTINGS, "Dock ayarları…"));
        }

        // Anchor beyond the icon (or the pointer on the dock itself).
        let (anchor, place) = match index {
            Some(i) => self.icon_anchor(i),
            None => {
                let s = self.scale();
                let mut rc = RECT::default();
                let mut pt = POINT::default();
                unsafe {
                    _ = GetWindowRect(self.hwnd.get(), &mut rc);
                    _ = GetCursorPos(&mut pt);
                }
                let along = match self.side() {
                    DockSide::Bottom => pt.x - rc.left,
                    _ => pt.y - rc.top,
                };
                self.anchor(along as f32 / s, self.headroom().1 - 6.0)
            }
        };
        self.hold(1); // stay out while the menu is open
        let dock: &'static Dock = unsafe { &*(self as *const Dock) };
        menu::show(entries, anchor, place, move |id| match id {
            NEW => {
                if let Some((launch, _)) = &launch {
                    unsafe {
                        ShellExecuteW(
                            None,
                            w!("open"),
                            &HSTRING::from(launch.as_str()),
                            None,
                            None,
                            SW_SHOWNORMAL,
                        )
                    };
                }
            }
            KEEP => {
                if let Some(i) = index {
                    dock.toggle_pin(i);
                }
            }
            LOCATION => {
                if let Some(exe) = &exe {
                    let args = HSTRING::from(format!("/select,\"{exe}\""));
                    unsafe {
                        ShellExecuteW(
                            None,
                            w!("open"),
                            w!("explorer.exe"),
                            &args,
                            None,
                            SW_SHOWNORMAL,
                        )
                    };
                }
            }
            CLOSE => windows.iter().for_each(|w| unsafe {
                _ = PostMessageW(Some(*w), WM_CLOSE, WPARAM(0), LPARAM(0))
            }),
            AUTOHIDE => settings::update(|st| st.dock_autohide = !st.dock_autohide),
            MAGNIFY => settings::update(|st| {
                st.dock_magnify = if st.dock_magnify > 1.0 { 1.0 } else { 1.5 }
            }),
            SETTINGS => crate::prefs::open(),
            n if n >= WINDOW => {
                if let Some(&w) = windows.get((n - WINDOW) as usize) {
                    unsafe {
                        if IsIconic(w).as_bool() {
                            _ = ShowWindow(w, SW_RESTORE);
                        }
                        _ = SetForegroundWindow(w);
                    }
                }
            }
            _ => {}
        });
        // The menu closes on its own; release the hold once it is gone.
        unsafe { SetTimer(Some(self.hwnd.get()), TIMER_PEEK, PEEK_MS, None) };
    }

    /// Right click on a folder or the Trash (macOS: Open, Remove from Dock, Empty Trash).
    fn stack_menu(&self, i: usize, kind: Kind, source: String) {
        const OPEN: u32 = 1;
        const REMOVE: u32 = 2;
        const EMPTY: u32 = 3;
        const SETTINGS: u32 = 4;
        let mut entries = vec![menu::Entry::item(OPEN, if kind == Kind::Trash { "Çöp Sepetini aç" } else { "Gezgin'de aç" })];
        match kind {
            Kind::Trash => entries.push(menu::Entry::item(EMPTY, "Çöp Sepetini boşalt…")),
            _ => entries.push(menu::Entry::item(REMOVE, "Dock'tan kaldır")),
        }
        entries.push(menu::Entry::Separator);
        entries.push(menu::Entry::item(SETTINGS, "Dock ayarları…"));
        let (x, place) = self.icon_anchor(i);
        self.hold(1);
        let dock: &'static Dock = unsafe { &*(self as *const Dock) };
        menu::show(entries, x, place, move |id| match id {
            OPEN => unsafe {
                ShellExecuteW(None, w!("open"), &HSTRING::from(source.as_str()), None, None, SW_SHOWNORMAL);
            },
            REMOVE => {
                let gone = source.clone();
                settings::update(|st| {
                    let mut f = settings::folders(st);
                    f.retain(|p| *p != gone);
                    st.folders = Some(f);
                });
            }
            // A second, explicit choice before deleting files for good.
            EMPTY => menu::show(
                vec![
                    menu::Entry::item(0, "Çöp Sepetindeki öğeler kalıcı olarak silinsin mi?"),
                    menu::Entry::Separator,
                    menu::Entry::item(1, "Çöp Sepetini boşalt"),
                    menu::Entry::item(2, "Vazgeç"),
                ],
                x,
                place,
                move |id| {
                    if id == 1 {
                        empty_trash();
                        dock.refresh_trash();
                    }
                },
            ),
            SETTINGS => crate::prefs::open(),
            _ => {}
        });
        unsafe { SetTimer(Some(self.hwnd.get()), TIMER_PEEK, PEEK_MS, None) };
    }

    /// Keeps or removes the app at `index`; the first change copies the
    /// Windows taskbar pins into our own list.
    fn toggle_pin(&self, index: usize) {
        let (pin, source, windows_empty) = {
            let items = self.items.borrow();
            (
                items[index].pin.clone(),
                items[index].source.clone(),
                items[index].windows.is_empty(),
            )
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
        let pins: Vec<String> = self
            .items
            .borrow()
            .iter()
            .filter_map(|i| i.pin.clone())
            .collect();
        settings::update(|st| st.pins = Some(pins));
    }

    /// Follows a settings change (from the menu or the settings window).
    pub fn apply(&self, before: &Settings, after: &Settings) {
        if settings::folders(before) != settings::folders(after) {
            self.sync_stacks();
        }
        let layout = (
            after.dock_icon,
            after.dock_autohide,
            after.dock_indicators,
            after.dock_magnify,
            after.dock_side,
        ) != (
            before.dock_icon,
            before.dock_autohide,
            before.dock_indicators,
            before.dock_magnify,
            before.dock_side,
        );
        if !layout {
            return;
        }
        if after.dock_autohide != before.dock_autohide {
            self.sync_appbar();
        }
        preview::hide();
        self.place();
        if !self.shown.get() {
            self.slide_to(true); // hidden docks slide out towards their new edge
        }
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
        // Pointer position along the dock (client px): x at the bottom, y at a side.
        let x = || match self.side() {
            DockSide::Bottom => (lp.0 & 0xFFFF) as i16 as i32,
            _ => ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
        };
        match msg {
            WM_OPEN_SETTINGS => crate::prefs::open(),
            m if m == self.shellhook && m != 0 => self.on_shell(winlist::decode(wp.0, lp.0)),
            WM_ICON => self.on_icon(unsafe { icons::take(lp) }),
            WM_RECHECK => self.recheck(HWND(wp.0 as *mut _)),
            WM_MOUSEMOVE => self.on_mouse_move(x()),
            WM_MOUSELEAVE => self.on_mouse_leave(),
            WM_TIMER if wp.0 == TIMER_HIDE => unsafe {
                _ = KillTimer(Some(hwnd), TIMER_HIDE);
                // Not while its menu or preview is in use.
                if self.autohide()
                    && !self.held.get()
                    && !menu::visible()
                    && !preview::visible()
                    && !popup::pointer_in(hwnd)
                {
                    self.slide_out();
                }
            },
            WM_TIMER if wp.0 == TIMER_FOLLOW => {
                unsafe { _ = KillTimer(Some(hwnd), TIMER_FOLLOW) };
                self.smooth.set(self.pointer.get().is_none());
            }
            WM_TIMER if wp.0 == TIMER_PREVIEW => {
                unsafe { _ = KillTimer(Some(hwnd), TIMER_PREVIEW) };
                if let Some(i) = self.hovered.get() {
                    self.show_preview(i);
                }
            }
            WM_TIMER if wp.0 == TIMER_PEEK => self.hold(0),
            WM_HOLD => self.hold(wp.0),
            WM_TIMER if wp.0 == TIMER_BOUNCE => self.end_launch_bounces(),
            WM_FULLSCREEN if !self.hidden => unsafe {
                preview::hide();
                _ = ShowWindow(hwnd, if wp.0 != 0 { SW_HIDE } else { SW_SHOWNOACTIVATE });
            },
            WM_TIMER if wp.0 == TIMER_REGION || wp.0 == TIMER_REST => {
                unsafe { _ = KillTimer(Some(hwnd), wp.0) };
                self.update_region();
            }
            WM_LBUTTONUP => {
                preview::hide();
                self.click(x(), false);
            }
            WM_MBUTTONUP => {
                preview::hide();
                self.click(x(), true);
            }
            WM_RBUTTONUP => self.context_menu(x()),
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_APPBAR if wp.0 as u32 == ABN_POSCHANGED => self.place(),
            WM_DPICHANGED | WM_DISPLAYCHANGE => self.place(),
            WM_SETTINGCHANGE => {
                if lp.0 == 0
                    || unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default()
                        != "ImmersiveColorSet"
                {
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

/// Short ease-out on Offset, Scale and Size: icons follow the pointer smoothly
/// and settle back when it leaves.
fn follow_animations(c: &Compositor) -> Result<ImplicitAnimationCollection> {
    let ease =
        c.CreateCubicBezierEasingFunction(Vector2 { X: 0.2, Y: 0.0 }, Vector2 { X: 0.0, Y: 1.0 })?;
    let set = c.CreateImplicitAnimationCollection()?;
    let duration = TimeSpan {
        Duration: FOLLOW_MS as i64 * 10_000,
    };
    for prop in ["Offset", "Scale"] {
        let a = c.CreateVector3KeyFrameAnimation()?;
        a.SetTarget(&HSTRING::from(prop))?;
        a.InsertExpressionKeyFrameWithEasingFunction(
            1.0,
            &HSTRING::from("this.FinalValue"),
            &ease,
        )?;
        a.SetDuration(duration)?;
        set.Insert(&HSTRING::from(prop), &a)?;
    }
    let size = c.CreateVector2KeyFrameAnimation()?;
    size.SetTarget(&HSTRING::from("Size"))?;
    size.InsertExpressionKeyFrameWithEasingFunction(1.0, &HSTRING::from("this.FinalValue"), &ease)?;
    size.SetDuration(duration)?;
    set.Insert(&HSTRING::from("Size"), &size)?;
    Ok(set)
}

unsafe extern "system" fn on_cloak(
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
                Some(DOCK_HWND.get()),
                WM_RECHECK,
                WPARAM(hwnd.0 as usize),
                LPARAM(0),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_docks_keep_the_edge_side() {
        // Window 100 px thick; a 20 px box touching the screen edge (c = 80).
        assert_eq!(turn(DockSide::Bottom, 100.0, 30.0, 80.0, 20.0), (30.0, 80.0));
        assert_eq!(turn(DockSide::Left, 100.0, 30.0, 80.0, 20.0), (0.0, 30.0), "left edge at x = 0");
        assert_eq!(turn(DockSide::Right, 100.0, 30.0, 80.0, 20.0), (80.0, 30.0), "right edge at x = 100");
        // The far side of the window points into the screen.
        assert_eq!(turn(DockSide::Left, 100.0, 0.0, 0.0, 20.0), (80.0, 0.0));
        assert_eq!(self_dims(DockSide::Left, 300.0, 100.0), (100.0, 300.0));
        assert_eq!(self_dims(DockSide::Bottom, 300.0, 100.0), (300.0, 100.0));
    }

    const G: Geometry = Geometry { icon: 48.0, sep: None };

    #[test]
    fn separator_adds_room_between_sections() {
        let g = Geometry { icon: 48.0, sep: Some(2) };
        assert_eq!(g.width(4), G.width(4) + g.sep_w());
        assert_eq!(g.item_x(1), G.item_x(1));
        assert_eq!(g.item_x(2), G.item_x(2) + g.sep_w());
        let only_stacks = Geometry { icon: 48.0, sep: Some(0) };
        assert_eq!(only_stacks.width(2), G.width(2), "no line without apps on the left");
    }

    #[test]
    fn geometry_scales_with_icon_size() {
        assert_eq!(
            (G.pad(), G.pad_bottom(), G.height(), G.radius()),
            (6.0, 10.0, 64.0, 18.0)
        );
        let small = Geometry { icon: 32.0, sep: None };
        assert_eq!((small.pad(), small.height()), (4.0, 44.0));
        let big = Geometry { icon: 64.0, sep: None };
        assert_eq!((big.pad(), big.height()), (8.0, 84.0));
    }

    #[test]
    fn width_grows_by_icon_and_gap() {
        assert_eq!(G.width(1), 2.0 * G.pad() + G.icon);
        assert_eq!(G.width(3) - G.width(2), G.icon + G.gap());
        assert_eq!(G.width(0), G.width(1), "an empty dock keeps one slot");
    }

    #[test]
    fn icon_source_prefers_package_then_exe() {
        let id = |a: Option<&str>, e: Option<&str>| AppIdentity {
            aumid: a.map(Into::into),
            exe: e.map(Into::into),
        };
        assert_eq!(
            source_for(&id(Some("Pkg_x!App"), Some("c:/x.exe"))).unwrap(),
            r"shell:AppsFolder\Pkg_x!App"
        );
        assert_eq!(
            source_for(&id(Some("Brave"), Some("c:/brave.exe"))).unwrap(),
            "c:/brave.exe"
        );
        assert_eq!(
            source_for(&id(Some("Microsoft.Windows.Explorer"), None)).unwrap(),
            r"shell:AppsFolder\Microsoft.Windows.Explorer"
        );
        assert!(source_for(&id(None, None)).is_none());
    }
}

/// Whether the Recycle Bin holds anything (decides its empty/full icon).
fn trash_full() -> bool {
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    let mut info = SHQUERYRBINFO { cbSize: size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.is_ok() && info.i64NumItems > 0
}

/// Empties the Recycle Bin without Windows' own confirmation, progress or sound
/// (the dock menu already asked).
fn empty_trash() {
    use windows::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW};
    unsafe { _ = SHEmptyRecycleBinW(None, PCWSTR::null(), SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND) };
}
