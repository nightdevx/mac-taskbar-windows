//! Launchpad: every installed app in a full-screen grid over the blurred
//! desktop, with search and pages, like macOS Launchpad. Opened from the dock
//! (the "show applications" button, as on Ubuntu).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use windows::Foundation::TimeSpan;
use windows::UI::Composition::{
    CompositionDrawingSurface, CompositionGeometricClip, CompositionRoundedRectangleGeometry, CompositionSurfaceBrush,
    Compositor, ContainerVisual, CubicBezierEasingFunction, ImplicitAnimationCollection, SpriteVisual,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, Result, w};
use windows_numerics::{Vector2, Vector3};

use crate::gfx::{self, Gfx, Label};
use crate::icons::{self, Loader};
use crate::popup::{self, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};

const CLASS: PCWSTR = w!("TaskbarLaunchpad");
const WM_ICON: u32 = WM_APP + 1;
/// The app list, read on a worker thread (enumerating the Apps folder takes ~1 s).
const WM_APPS: u32 = WM_APP + 2;
/// Build the neighbouring pages after the current one is on screen.
const WM_NEIGHBOURS: u32 = WM_APP + 3;
pub const COLS: usize = 7;
pub const ROWS: usize = 5;
pub const PER_PAGE: usize = COLS * ROWS;
const SEARCH_W: f32 = 240.0;
const SEARCH_H: f32 = 30.0;
const SEARCH_TOP: f32 = 48.0;

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub name: String,
    /// Apps folder parsing name: an AUMID or a known-folder path.
    pub id: String,
}

impl App {
    fn source(&self) -> String {
        format!(r"shell:AppsFolder\{}", self.id)
    }

    /// Where the icon comes from: the exe itself when the ID is a known-folder
    /// path ("{GUID}\Core Temp\Core Temp.exe"), because the Apps folder draws
    /// exes that only carry small icons inside a grey frame.
    pub fn icon_source(&self) -> String {
        exe_path(&self.id).unwrap_or_else(|| self.source())
    }
}

/// "{6D809377-...}\Dirpp.exe" to a full exe path.
fn exe_path(id: &str) -> Option<String> {
    use windows::Win32::UI::Shell::SHGetKnownFolderPath;
    let rest = id.strip_prefix('{')?;
    let (guid, path) = rest.split_once("}\\")?;
    if !path.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    let guid = windows::core::GUID::try_from(guid).ok()?;
    unsafe {
        let base = SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, None).ok()?;
        let dir = base.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(base.0 as *const _));
        Some(format!(r"{}\{}", dir?, path))
    }
}

/// Every app the Start menu lists, by name.
pub fn apps() -> Vec<App> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) else {
            return out;
        };
        let Ok(en) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut item = [None];
            let mut fetched = 0;
            if en.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = item[0].take() else { break };
            let text = |kind| {
                item.GetDisplayName(kind).ok().and_then(|p| {
                    let s = p.to_string().ok();
                    windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
                    s
                })
            };
            if let (Some(name), Some(id)) = (text(SIGDN_NORMALDISPLAY), text(SIGDN_PARENTRELATIVEPARSING)) {
                // Manuals, readmes and web links share the Start menu; Launchpad shows apps.
                const DOCS: [&str; 8] = [".txt", ".chm", ".pdf", ".htm", ".html", ".url", ".rtf", ".md"];
                let lower = id.to_ascii_lowercase();
                if !DOCS.iter().any(|ext| lower.ends_with(ext)) {
                    out.push(App { name, id });
                }
            }
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Indexes of apps whose name contains `query` (case-insensitive).
pub fn filter(apps: &[App], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    (0..apps.len()).filter(|i| q.is_empty() || apps[*i].name.to_lowercase().contains(&q)).collect()
}

/// Apps the current view lists: visible ones, or only the hidden ones.
pub fn listed(apps: &[App], query: &str, hidden: &[String], show_hidden: bool) -> Vec<usize> {
    filter(apps, query)
        .into_iter()
        .filter(|i| hidden.iter().any(|h| *h == apps[*i].id) == show_hidden)
        .collect()
}

/// Pill buttons in the top row (DIPs): Edit/Done, Hide/Show (while editing),
/// and the Hidden apps toggle on the right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Edit,
    Apply,
    HiddenView,
}

const PILL_H: f32 = 30.0;

pub fn button_rect(w: f32, h: f32, b: Button) -> D2D_RECT_F {
    let g = grid(w, h);
    let right = g.left + COLS as f32 * g.cell_w;
    let (left, width) = match b {
        Button::Edit => (g.left, 92.0),
        Button::Apply => (g.left + 100.0, 120.0),
        Button::HiddenView => (right - 170.0, 170.0),
    };
    D2D_RECT_F { left, top: SEARCH_TOP, right: left + width, bottom: SEARCH_TOP + PILL_H }
}

pub fn hit_button(w: f32, h: f32, editing: bool, x: f32, y: f32) -> Option<Button> {
    [Button::Edit, Button::Apply, Button::HiddenView]
        .into_iter()
        .filter(|b| editing || *b != Button::Apply)
        .find(|b| {
            let r = button_rect(w, h, *b);
            x >= r.left && x < r.right && y >= r.top && y < r.bottom
        })
}

pub fn pages(n: usize) -> usize {
    n.div_ceil(PER_PAGE).max(1)
}

/// Grid placement for a `w` x `h` DIP screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub left: f32,
    pub top: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub icon: f32,
}

pub fn grid(w: f32, h: f32) -> Grid {
    let grid_w = (w * 0.78).min(COLS as f32 * 150.0);
    let top = SEARCH_TOP + SEARCH_H + 44.0;
    let cell_w = grid_w / COLS as f32;
    let cell_h = ((h - 90.0 - top) / ROWS as f32).max(60.0);
    Grid { left: (w - grid_w) / 2.0, top, cell_w, cell_h, icon: (cell_w * 0.55).min(cell_h * 0.6).min(88.0).round() }
}

/// Position on the page of the app under (x, y).
pub fn hit(g: Grid, on_page: usize, x: f32, y: f32) -> Option<usize> {
    if x < g.left || y < g.top {
        return None;
    }
    let (c, r) = (((x - g.left) / g.cell_w) as usize, ((y - g.top) / g.cell_h) as usize);
    let i = r * COLS + c;
    (c < COLS && r < ROWS && i < on_page).then_some(i)
}

fn dots_y(h: f32) -> f32 {
    h - 50.0
}

/// Page dot under (x, y).
pub fn hit_dot(w: f32, h: f32, n_pages: usize, x: f32, y: f32) -> Option<usize> {
    if n_pages < 2 || (y - dots_y(h)).abs() > 10.0 {
        return None;
    }
    let first = w / 2.0 - (n_pages - 1) as f32 * 9.0;
    (0..n_pages).find(|i| (x - (first + *i as f32 * 18.0)).abs() <= 8.0)
}

pub struct Fonts {
    pub label: IDWriteTextFormat,
    pub text: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
    pub cap: f32,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory) -> Result<Self> {
        let label = gfx::text_format(dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_REGULAR)?;
        unsafe { label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)? };
        Ok(Fonts {
            label,
            text: gfx::text_format(dwrite, theme::TEXT_FONT, 14.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            icons: gfx::text_format(dwrite, theme::ICON_FONT, 14.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            cap: gfx::cap_height(dwrite, theme::TEXT_FONT, 14.0)?,
        })
    }
}

/// What one frame of Launchpad shows.
pub struct View<'a> {
    pub size: (f32, f32),
    pub query: &'a str,
    /// Apps on the current page.
    pub apps: &'a [App],
    pub icons: &'a HashMap<String, icons::Icon>,
    pub loading: bool,
    pub page: usize,
    pub pages: usize,
    pub hover: Option<usize>,
    pub scale: f32,
    /// Selection mode: icons get check circles, clicks select.
    pub editing: bool,
    /// Selected app IDs (while editing).
    pub selected: &'a HashSet<String>,
    /// Listing hidden apps instead of visible ones.
    pub show_hidden: bool,
    pub hidden_count: usize,
    /// Live Launchpad: apps are composition tiles, the surface holds the rest.
    pub chrome_only: bool,
}

pub fn draw(dc: &ID2D1DeviceContext, dwrite: &IDWriteFactory, f: &Fonts, v: &View) -> Result<()> {
    let (w, h) = v.size;
    let g = grid(w, h);
    unsafe {
        let white = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 240).d2d(), None)?;
        let shadow = dc.CreateSolidColorBrush(&Rgba(0, 0, 0, 110).d2d(), None)?;
        let field = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 46).d2d(), None)?;
        let dim = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 150).d2d(), None)?;
        let hover = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 34).d2d(), None)?;
        let accent = dc.CreateSolidColorBrush(&theme::accent().d2d(), None)?;
        let ring = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 200).d2d(), None)?;

        // Top row pills: Edit/Done, Hide/Show (k), Hidden apps.
        let pill = |b: Button, label: &str, strong: bool| -> Result<()> {
            let r = button_rect(w, h, b);
            let fill = if strong { &accent } else { &field };
            dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: PILL_H / 2.0, radiusY: PILL_H / 2.0 }, fill);
            let l = Label::fitted(dwrite, label, &f.text, r.right - r.left - 20.0)?;
            l.draw_text(dc, (r.left + r.right - l.width) / 2.0, r.top + r.bottom, f.cap, &white);
            Ok(())
        };
        pill(Button::Edit, if v.editing { "Bitti" } else { "Düzenle" }, v.editing)?;
        if v.editing {
            let k = v.selected.len();
            let label = if v.show_hidden { format!("Göster ({k})") } else { format!("Gizle ({k})") };
            pill(Button::Apply, &label, k > 0)?;
        }
        let hidden_label = if v.show_hidden { "Tüm uygulamalar".to_string() } else { format!("Gizlenenler ({})", v.hidden_count) };
        pill(Button::HiddenView, &hidden_label, v.show_hidden)?;

        // Search field, centred at the top.
        let sl = (w - SEARCH_W) / 2.0;
        let search = D2D_RECT_F { left: sl, top: SEARCH_TOP, right: sl + SEARCH_W, bottom: SEARCH_TOP + SEARCH_H };
        dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: search, radiusX: 8.0, radiusY: 8.0 }, &field);
        let band = SEARCH_TOP * 2.0 + SEARCH_H;
        Label::new(dwrite, "\u{E721}", &f.icons)?.draw(dc, sl + 10.0, band, &dim);
        let q = v.query;
        if q.is_empty() {
            Label::new(dwrite, "Ara", &f.text)?.draw_text(dc, sl + 34.0, band, f.cap, &dim);
        } else {
            Label::fitted(dwrite, &q, &f.text, SEARCH_W - 44.0)?.draw_text(dc, sl + 34.0, band, f.cap, &white);
        }

        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0 * v.scale,
            dpiY: 96.0 * v.scale,
            ..Default::default()
        };
        let icons = v.icons;
        let apps = v.apps;
        if apps.is_empty() {
            let msg = if v.loading {
                "Uygulamalar yükleniyor…"
            } else if v.show_hidden && v.query.is_empty() {
                "Gizlenen uygulama yok"
            } else {
                "Sonuç yok"
            };
            let l = Label::new(dwrite, msg, &f.text)?;
            l.draw_text(dc, (w - l.width) / 2.0, h, f.cap, &dim);
        }
        for (i, app) in apps.iter().enumerate().filter(|_| !v.chrome_only) {
            let (c, r) = ((i % COLS) as f32, (i / COLS) as f32);
            let cx = g.left + (c + 0.5) * g.cell_w;
            let top = g.top + r * g.cell_h + 6.0;
            if v.hover == Some(i) {
                let cell = D2D_RECT_F { left: cx - g.cell_w / 2.0 + 6.0, top: top - 4.0, right: cx + g.cell_w / 2.0 - 6.0, bottom: top + g.cell_h - 10.0 };
                dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: cell, radiusX: 14.0, radiusY: 14.0 }, &hover);
            }
            if let Some(icon) = icons.get(&app.icon_source()).filter(|i| !i.pixels.is_empty()) {
                let bmp = dc.CreateBitmap(D2D_SIZE_U { width: icon.size, height: icon.size }, Some(icon.pixels.as_ptr() as *const _), icon.size * 4, &props)?;
                let rect = D2D_RECT_F { left: cx - g.icon / 2.0, top, right: cx + g.icon / 2.0, bottom: top + g.icon };
                dc.DrawBitmap(&bmp, Some(&rect), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
            }
            if v.editing {
                // macOS-style selection circle on the icon's top-left corner.
                let c = Vector2 { X: cx - g.icon / 2.0 + 4.0, Y: top + 4.0 };
                let e = D2D1_ELLIPSE { point: c, radiusX: 11.0, radiusY: 11.0 };
                if v.selected.contains(&app.id) {
                    dc.FillEllipse(&e, &accent);
                    dc.DrawEllipse(&e, &white, 1.5, None);
                    let mark = Label::new(dwrite, "\u{E73E}", &f.icons)?;
                    mark.draw(dc, c.X - mark.width / 2.0, c.Y * 2.0, &white);
                } else {
                    dc.FillEllipse(&e, &shadow);
                    dc.DrawEllipse(&e, &ring, 1.5, None);
                }
            }
            // Name under the icon, white with a soft shadow for any wallpaper.
            let wide: Vec<u16> = app.name.encode_utf16().collect();
            let layout = dwrite.CreateTextLayout(&wide, &f.label, g.cell_w - 12.0, 20.0)?;
            layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
            layout.SetTrimming(&trim, &dwrite.CreateEllipsisTrimmingSign(&f.label)?)?;
            let origin = Vector2 { X: cx - (g.cell_w - 12.0) / 2.0, Y: top + g.icon + 8.0 };
            dc.DrawTextLayout(Vector2 { X: origin.X, Y: origin.Y + 1.0 }, &layout, &shadow, D2D1_DRAW_TEXT_OPTIONS_NONE);
            dc.DrawTextLayout(origin, &layout, &white, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }

        let n = v.pages;
        if n > 1 {
            let first = w / 2.0 - (n - 1) as f32 * 9.0;
            for p in 0..n {
                let brush = if p == v.page { &white } else { &dim };
                let e = D2D1_ELLIPSE { point: Vector2 { X: first + p as f32 * 18.0, Y: dots_y(h) }, radiusX: 3.5, radiusY: 3.5 };
                dc.FillEllipse(&e, brush);
            }
        }
    }
    Ok(())
}


/// Page after a drag of `drag` px (negative = content pulled left): a fifth of
/// the screen turns the page, like macOS.
pub fn snap(page: usize, n_pages: usize, drag: f32, width: f32) -> usize {
    let last = n_pages.saturating_sub(1);
    if drag < -width * 0.2 {
        (page + 1).min(last)
    } else if drag > width * 0.2 {
        page.saturating_sub(1)
    } else {
        page.min(last)
    }
}

const TIMER_SNAP: usize = 1;
const TIMER_CLOSE: usize = 2;
const SLIDE_MS: i64 = 380;
const OPEN_MS: i64 = 320;
const CLOSE_MS: i64 = 200;
const BADGE: f32 = 24.0;

/// One app on a page: icon, name and (while editing) a selection badge.
struct Tile {
    id: String,
    source: String,
    root: ContainerVisual,
    icon: SpriteVisual,
    badge: SpriteVisual,
}

struct Comp {
    /// All pages side by side; its offset picks the page (animated slide).
    pages: ContainerVisual,
    hover: SpriteVisual,
    slide: ImplicitAnimationCollection,
    still: ImplicitAnimationCollection,
    badge_on: CompositionSurfaceBrush,
    badge_off: CompositionSurfaceBrush,
}

struct Launchpad {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    fonts: Fonts,
    loader: OnceCell<Loader>,
    comp: RefCell<Option<Comp>>,
    apps: RefCell<Vec<App>>,
    shown: RefCell<Vec<usize>>,
    query: RefCell<String>,
    page: Cell<usize>,
    hover: Cell<Option<usize>>,
    editing: Cell<bool>,
    selected: RefCell<HashSet<String>>,
    show_hidden: Cell<bool>,
    /// Icon brushes by source, kept between openings so icons show at once.
    brushes: RefCell<HashMap<String, CompositionSurfaceBrush>>,
    requested: RefCell<HashSet<String>>,
    tiles: RefCell<HashMap<usize, Vec<Tile>>>,
    size: Cell<(f32, f32)>,
    scale: Cell<f32>,
    icon_px: Cell<u32>,
    /// Horizontal drag of the pages in px (two-finger scroll or three-finger swipe).
    drag: Cell<f32>,
    dragging: Cell<bool>,
    closing: Cell<bool>,
    touch: RefCell<crate::touchpad::Swipe>,
}

thread_local! {
    static LP: OnceCell<&'static Launchpad> = const { OnceCell::new() };
}

fn get() -> Option<&'static Launchpad> {
    LP.with(|l| l.get().copied())
}

fn ease(c: &Compositor) -> Result<CubicBezierEasingFunction> {
    c.CreateCubicBezierEasingFunction(Vector2 { X: 0.2, Y: 0.0 }, Vector2 { X: 0.0, Y: 1.0 })
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let lp: &'static Launchpad = Box::leak(Box::new(Launchpad {
        fonts: Fonts::new(&gfx.dwrite)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        loader: OnceCell::new(),
        comp: RefCell::new(None),
        apps: RefCell::new(Vec::new()),
        shown: RefCell::new(Vec::new()),
        query: RefCell::new(String::new()),
        page: Cell::new(0),
        hover: Cell::new(None),
        editing: Cell::new(false),
        selected: RefCell::new(HashSet::new()),
        show_hidden: Cell::new(false),
        brushes: RefCell::new(HashMap::new()),
        requested: RefCell::new(HashSet::new()),
        tiles: RefCell::new(HashMap::new()),
        size: Cell::new((0.0, 0.0)),
        scale: Cell::new(1.0),
        icon_px: Cell::new(64),
        drag: Cell::new(0.0),
        dragging: Cell::new(false),
        closing: Cell::new(false),
        touch: RefCell::new(crate::touchpad::Swipe::default()),
    }));
    let popup = Popup::create(gfx.clone(), CLASS, true, lp)?;
    // Three icon workers: a page of 35 icons arrives in a third of the time.
    _ = lp.loader.set(Loader::pool(popup.hwnd, WM_ICON, 3));
    let layer = popup.layer().expect("popup layer");
    _ = lp.popup.set(popup);
    lp.init_visuals(&layer)?;
    LP.with(|l| _ = l.set(lp));
    // Read the app list now, in the background, so the first opening is instant.
    refresh_apps(lp.popup().hwnd);
    Ok(())
}

/// Reads the app list on a worker thread and posts it to Launchpad.
fn refresh_apps(target: HWND) {
    let target = target.0 as isize;
    std::thread::spawn(move || unsafe {
        _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        let list = Box::into_raw(Box::new(apps()));
        if PostMessageW(Some(HWND(target as *mut _)), WM_APPS, WPARAM(0), LPARAM(list as isize)).is_err() {
            drop(Box::from_raw(list));
        }
    });
}

/// Opens Launchpad, or closes it when it is open (the dock button toggles).
pub fn toggle() {
    let Some(lp) = get() else { return };
    if lp.popup().visible() {
        return lp.close();
    }
    lp.open();
}

impl Launchpad {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("launchpad popup")
    }

    fn init_visuals(&self, layer: &ContainerVisual) -> Result<()> {
        let c = &self.gfx.compositor;
        let pages = c.CreateContainerVisual()?;
        layer.Children()?.InsertAtTop(&pages)?;
        let hover = c.CreateSpriteVisual()?;
        hover.SetBrush(&c.CreateColorBrushWithColor(Rgba(255, 255, 255, 34).ui())?)?;
        let shape = c.CreateRoundedRectangleGeometry()?;
        shape.SetCornerRadius(Vector2 { X: 14.0, Y: 14.0 })?;
        hover.SetClip(&c.CreateGeometricClipWithGeometry(&shape)?)?;
        hover.SetIsVisible(false)?;
        pages.Children()?.InsertAtBottom(&hover)?;

        let slide = c.CreateImplicitAnimationCollection()?;
        let a = c.CreateVector3KeyFrameAnimation()?;
        a.SetTarget(&HSTRING::from("Offset"))?;
        a.InsertExpressionKeyFrameWithEasingFunction(1.0, &HSTRING::from("this.FinalValue"), &ease(c)?)?;
        a.SetDuration(TimeSpan { Duration: SLIDE_MS * 10_000 })?;
        slide.Insert(&HSTRING::from("Offset"), &a)?;
        let still = c.CreateImplicitAnimationCollection()?;

        // Selection badges, drawn once: filled with a tick, or an empty ring.
        let (on, off) = (self.gfx.surface(1, 1)?, self.gfx.surface(1, 1)?);
        let badge_on = c.CreateSurfaceBrushWithSurface(&on)?;
        let badge_off = c.CreateSurfaceBrushWithSurface(&off)?;
        *self.comp.borrow_mut() = Some(Comp { pages, hover, slide, still, badge_on, badge_off });
        let _ = (on, off);
        Ok(())
    }

    fn draw_badges(&self) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let s = self.scale.get();
        let px = (BADGE * s).ceil() as u32;
        for (brush, selected) in [(&c.badge_on, true), (&c.badge_off, false)] {
            let Ok(surface) = brush.Surface().and_then(|x| x.cast::<CompositionDrawingSurface>()) else { continue };
            let accent = theme::accent();
            let icons = &self.fonts.icons;
            let dwrite = &self.gfx.dwrite;
            let r = self.gfx.resize(&surface, px, px).and_then(|_| {
                self.gfx.draw(&surface, s, |dc| unsafe {
                    let e = D2D1_ELLIPSE { point: Vector2 { X: BADGE / 2.0, Y: BADGE / 2.0 }, radiusX: 10.5, radiusY: 10.5 };
                    let white = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 235).d2d(), None)?;
                    if selected {
                        dc.FillEllipse(&e, &dc.CreateSolidColorBrush(&accent.d2d(), None)?);
                        dc.DrawEllipse(&e, &white, 1.5, None);
                        let mark = Label::new(dwrite, "\u{E73E}", icons)?;
                        mark.draw(dc, (BADGE - mark.width) / 2.0, BADGE, &white);
                    } else {
                        dc.FillEllipse(&e, &dc.CreateSolidColorBrush(&Rgba(0, 0, 0, 110).d2d(), None)?);
                        dc.DrawEllipse(&e, &white, 1.5, None);
                    }
                    Ok(())
                })
            });
            if let Err(e) = r {
                crate::log::error(&format!("launchpad badge: {e}"));
            }
        }
    }

    fn open(&self) {
        let (monitor, dpi) = window::primary_monitor();
        let s = theme::scale(dpi);
        let (w, h) = ((monitor.right - monitor.left) as f32 / s, (monitor.bottom - monitor.top) as f32 / s);
        let new_px = (grid(w, h).icon * s).round() as u32;
        if new_px != self.icon_px.get() {
            // Different screen or scale: cached icons are the wrong size.
            self.brushes.borrow_mut().clear();
            self.requested.borrow_mut().clear();
        }
        self.size.set((w, h));
        self.scale.set(s);
        self.icon_px.set(new_px);
        self.closing.set(false);
        self.query.borrow_mut().clear();
        self.editing.set(false);
        self.selected.borrow_mut().clear();
        self.show_hidden.set(false);
        self.page.set(0);
        self.hover.set(None);
        self.drag.set(0.0);
        self.draw_badges();
        self.refilter();
        let pivot = (monitor.bottom - monitor.top) / 2;
        self.popup().set_tint(Rgba(18, 18, 22, 120));
        self.popup().show_rect(monitor, dpi, 0.0, pivot, |dc| self.draw_chrome(dc));
        self.popup().set_tint(Rgba(18, 18, 22, 120));
        self.animate_pages(true);
        crate::touchpad::listen(self.popup().hwnd, true);
        refresh_apps(self.popup().hwnd); // picks up newly installed apps
    }

    /// macOS: icons zoom in from slightly larger and fade in; reversed on close.
    fn animate_pages(&self, opening: bool) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let comp_ = &self.gfx.compositor;
        let (w, h) = self.size.get();
        let s = self.scale.get();
        if !theme::animations() {
            _ = c.pages.StopAnimation(&HSTRING::from("Scale"));
            _ = c.pages.StopAnimation(&HSTRING::from("Opacity"));
            _ = c.pages.SetScale(Vector3 { X: 1.0, Y: 1.0, Z: 1.0 });
            _ = c.pages.SetOpacity(1.0);
            return;
        }
        let run = || -> Result<()> {
            let ms = if opening { OPEN_MS } else { CLOSE_MS };
            let (from, to) = if opening { (1.1, 1.0) } else { (1.0, 1.08) };
            let zoom = comp_.CreateVector3KeyFrameAnimation()?;
            zoom.InsertKeyFrame(0.0, Vector3 { X: from, Y: from, Z: 1.0 })?;
            zoom.InsertKeyFrameWithEasingFunction(1.0, Vector3 { X: to, Y: to, Z: 1.0 }, &ease(comp_)?)?;
            zoom.SetDuration(TimeSpan { Duration: ms * 10_000 })?;
            let fade = comp_.CreateScalarKeyFrameAnimation()?;
            fade.InsertKeyFrame(0.0, if opening { 0.0 } else { 1.0 })?;
            fade.InsertKeyFrame(1.0, if opening { 1.0 } else { 0.0 })?;
            fade.SetDuration(TimeSpan { Duration: ms * 10_000 })?;
            // Zoom around the screen centre of the current page.
            let page_x = self.page.get() as f32 * w * s;
            c.pages.SetCenterPoint(Vector3 { X: page_x + w * s / 2.0, Y: h * s / 2.0, Z: 0.0 })?;
            c.pages.StartAnimation(&HSTRING::from("Scale"), &zoom)?;
            c.pages.StartAnimation(&HSTRING::from("Opacity"), &fade)
        };
        _ = run();
    }

    fn close(&self) {
        if self.closing.replace(true) {
            return;
        }
        crate::touchpad::listen(self.popup().hwnd, false);
        if !theme::animations() {
            return self.finish_close();
        }
        self.animate_pages(false);
        self.popup().fade_out(CLOSE_MS);
        unsafe { SetTimer(Some(self.popup().hwnd), TIMER_CLOSE, CLOSE_MS as u32, None) };
    }

    fn finish_close(&self) {
        self.popup().hide_and_release();
        self.clear_tiles();
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.pages.SetImplicitAnimations(&c.still);
            _ = c.pages.SetOffset(Vector3::default());
            _ = c.hover.SetIsVisible(false);
        }
        self.closing.set(false);
    }

    fn clear_tiles(&self) {
        let comp = self.comp.borrow();
        for (_, tiles) in self.tiles.borrow_mut().drain() {
            for t in tiles {
                if let Some(c) = comp.as_ref() {
                    _ = c.pages.Children().and_then(|ch| ch.Remove(&t.root));
                }
            }
        }
    }

    fn refilter(&self) {
        let hidden = crate::settings::get().hidden_apps;
        let shown = listed(&self.apps.borrow(), &self.query.borrow(), &hidden, self.show_hidden.get());
        *self.shown.borrow_mut() = shown;
        self.page.set(self.page.get().min(pages(self.shown.borrow().len()) - 1));
        self.clear_tiles();
        self.place_pages(false);
    }

    fn apps_on(&self, page: usize) -> Vec<App> {
        let apps = self.apps.borrow();
        self.shown.borrow().iter().skip(page * PER_PAGE).take(PER_PAGE).map(|i| apps[*i].clone()).collect()
    }

    fn page_apps(&self) -> Vec<App> {
        self.apps_on(self.page.get())
    }

    /// Builds tiles for the current page and its neighbours, moves the pages
    /// container to the current page (sliding when `animate`) and asks for icons.
    fn place_pages(&self, animate: bool) {
        let p = self.page.get();
        self.ensure_page(p);
        unsafe { _ = PostMessageW(Some(self.popup().hwnd), WM_NEIGHBOURS, WPARAM(0), LPARAM(0)) };
        let (w, _) = self.size.get();
        let s = self.scale.get();
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.pages.SetImplicitAnimations(if animate && theme::animations() { &c.slide } else { &c.still });
            _ = c.pages.SetOffset(Vector3 { X: -(p as f32) * w * s, Y: 0.0, Z: 0.0 });
            _ = c.hover.SetIsVisible(false);
        }
        self.hover.set(None);
        self.request(p);
        self.request(p + 1);
        self.redraw_chrome();
    }

    fn ensure_page(&self, page: usize) {
        if self.tiles.borrow().contains_key(&page) {
            return;
        }
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let (w, h) = self.size.get();
        let g = grid(w, h);
        let s = self.scale.get();
        let cmp = &self.gfx.compositor;
        let mut tiles = Vec::new();
        for (i, app) in self.apps_on(page).into_iter().enumerate() {
            let make = || -> Result<Tile> {
                let (col, row) = ((i % COLS) as f32, (i / COLS) as f32);
                let left = page as f32 * w + g.left + col * g.cell_w;
                let top = g.top + row * g.cell_h + 6.0;
                let root = cmp.CreateContainerVisual()?;
                root.SetOffset(Vector3 { X: (left * s).round(), Y: (top * s).round(), Z: 0.0 })?;
                let icon = cmp.CreateSpriteVisual()?;
                let ipx = (g.icon * s).round();
                icon.SetSize(Vector2 { X: ipx, Y: ipx })?;
                icon.SetOffset(Vector3 { X: (((g.cell_w - g.icon) / 2.0) * s).round(), Y: 0.0, Z: 0.0 })?;
                let source = app.icon_source();
                if let Some(b) = self.brushes.borrow().get(&source) {
                    icon.SetBrush(b)?;
                }
                let label = cmp.CreateSpriteVisual()?;
                let (lw, lh) = (g.cell_w - 12.0, 20.0);
                let surface = self.gfx.surface((lw * s).ceil() as u32, (lh * s).ceil() as u32)?;
                let name = app.name.clone();
                let (label_fmt, dwrite) = (&self.fonts.label, &self.gfx.dwrite);
                self.gfx.draw(&surface, s, |dc| unsafe {
                    let wide: Vec<u16> = name.encode_utf16().collect();
                    let layout = dwrite.CreateTextLayout(&wide, label_fmt, lw, lh)?;
                    layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                    let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                    layout.SetTrimming(&trim, &dwrite.CreateEllipsisTrimmingSign(label_fmt)?)?;
                    let shadow = dc.CreateSolidColorBrush(&Rgba(0, 0, 0, 110).d2d(), None)?;
                    let white = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 240).d2d(), None)?;
                    dc.DrawTextLayout(Vector2 { X: 0.0, Y: 1.0 }, &layout, &shadow, D2D1_DRAW_TEXT_OPTIONS_NONE);
                    dc.DrawTextLayout(Vector2 { X: 0.0, Y: 0.0 }, &layout, &white, D2D1_DRAW_TEXT_OPTIONS_NONE);
                    Ok(())
                })?;
                label.SetBrush(&cmp.CreateSurfaceBrushWithSurface(&surface)?)?;
                label.SetSize(Vector2 { X: (lw * s).ceil(), Y: (lh * s).ceil() })?;
                label.SetOffset(Vector3 { X: (6.0 * s).round(), Y: ((g.icon + 8.0) * s).round(), Z: 0.0 })?;
                let badge = cmp.CreateSpriteVisual()?;
                let bpx = (BADGE * s).ceil();
                badge.SetSize(Vector2 { X: bpx, Y: bpx })?;
                badge.SetOffset(Vector3 { X: (((g.cell_w - g.icon) / 2.0 - BADGE / 2.0 + 4.0) * s).round(), Y: ((4.0 - BADGE / 2.0) * s).round(), Z: 0.0 })?;
                badge.SetBrush(if self.selected.borrow().contains(&app.id) { &c.badge_on } else { &c.badge_off })?;
                badge.SetIsVisible(self.editing.get())?;
                root.Children()?.InsertAtTop(&icon)?;
                root.Children()?.InsertAtTop(&label)?;
                root.Children()?.InsertAtTop(&badge)?;
                c.pages.Children()?.InsertAtTop(&root)?;
                Ok(Tile { id: app.id.clone(), source, root, icon, badge })
            };
            match make() {
                Ok(t) => tiles.push(t),
                Err(e) => crate::log::error(&format!("launchpad tile: {e}")),
            }
        }
        drop(comp);
        self.tiles.borrow_mut().insert(page, tiles);
    }

    /// Asks the workers for a page's icons that are not cached yet.
    fn request(&self, page: usize) {
        let Some(loader) = self.loader.get() else { return };
        for app in self.apps_on(page) {
            let src = app.icon_source();
            if !self.brushes.borrow().contains_key(&src) && self.requested.borrow_mut().insert(src.clone()) {
                loader.request(&src, self.icon_px.get());
            }
        }
    }

    fn on_icon(&self, icon: Box<icons::Icon>) {
        if icon.pixels.is_empty() || icon.size != self.icon_px.get() {
            return;
        }
        let Ok(brush) = self.gfx.icon_brush(&icon) else { return };
        for t in self.tiles.borrow().values().flatten().filter(|t| t.source == icon.source) {
            _ = t.icon.SetBrush(&brush);
        }
        self.brushes.borrow_mut().insert(icon.source.clone(), brush);
    }

    fn draw_chrome(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let page = self.page_apps();
        let none = HashMap::new();
        let view = View {
            size: self.size.get(),
            query: &self.query.borrow(),
            apps: &page,
            icons: &none,
            loading: self.apps.borrow().is_empty(),
            page: self.page.get(),
            pages: pages(self.shown.borrow().len()),
            hover: None,
            scale: self.scale.get(),
            editing: self.editing.get(),
            selected: &self.selected.borrow(),
            show_hidden: self.show_hidden.get(),
            hidden_count: crate::settings::get().hidden_apps.len(),
            chrome_only: true,
        };
        draw(dc, &self.gfx.dwrite, &self.fonts, &view)
    }

    fn redraw_chrome(&self) {
        if self.popup().visible() {
            self.popup().redraw(|dc| self.draw_chrome(dc));
        }
    }

    fn set_page(&self, page: usize) {
        let n = pages(self.shown.borrow().len());
        let page = page.min(n - 1);
        self.page.set(page);
        self.place_pages(true);
    }

    // ---- dragging (two-finger scroll, three-finger swipe) ----

    fn drag_by(&self, dx: f32) {
        let (w, _) = self.size.get();
        let s = self.scale.get();
        let width = w * s;
        if !self.dragging.replace(true) {
            self.drag.set(0.0);
        }
        // Rubber band at the first and last page.
        let n = pages(self.shown.borrow().len());
        let mut d = (self.drag.get() + dx).clamp(-width, width);
        let p = self.page.get();
        if (p == 0 && d > 0.0) || (p + 1 >= n && d < 0.0) {
            d = d.signum() * (d.abs() / 3.0).min(width * 0.15);
        }
        self.drag.set(d);
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = c.pages.SetImplicitAnimations(&c.still);
            _ = c.pages.SetOffset(Vector3 { X: -(p as f32) * width + d, Y: 0.0, Z: 0.0 });
            _ = c.hover.SetIsVisible(false);
        }
        unsafe { SetTimer(Some(self.popup().hwnd), TIMER_SNAP, 140, None) };
    }

    fn release_drag(&self) {
        if !self.dragging.replace(false) {
            return;
        }
        let (w, _) = self.size.get();
        let width = w * self.scale.get();
        let target = snap(self.page.get(), pages(self.shown.borrow().len()), self.drag.take(), width);
        self.set_page(target);
    }

    fn set_hover(&self, h: Option<usize>) {
        if self.hover.replace(h) == h {
            return;
        }
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        match h {
            Some(i) if !self.dragging.get() => {
                let (w, hh) = self.size.get();
                let g = grid(w, hh);
                let s = self.scale.get();
                let (col, row) = ((i % COLS) as f32, (i / COLS) as f32);
                let left = self.page.get() as f32 * w + g.left + col * g.cell_w + 6.0;
                let top = g.top + row * g.cell_h + 2.0;
                let size = Vector2 { X: ((g.cell_w - 12.0) * s).round(), Y: ((g.cell_h - 6.0) * s).round() };
                _ = c.hover.SetSize(size);
                if let Ok(clip) = c.hover.Clip().and_then(|x| x.cast::<CompositionGeometricClip>())
                    && let Ok(geo) = clip.Geometry().and_then(|x| x.cast::<CompositionRoundedRectangleGeometry>())
                {
                    _ = geo.SetSize(size);
                }
                _ = c.hover.SetOffset(Vector3 { X: (left * s).round(), Y: (top * s).round(), Z: 0.0 });
                _ = c.hover.SetIsVisible(true);
            }
            _ => _ = c.hover.SetIsVisible(false),
        }
    }

    fn launch(&self, app: &App) {
        unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(app.source()), None, None, SW_SHOWNORMAL) };
        self.close();
    }

    fn click(&self, x: f32, y: f32) {
        let (w, h) = self.size.get();
        if let Some(b) = hit_button(w, h, self.editing.get(), x, y) {
            return self.press(b);
        }
        let page = self.page_apps();
        if let Some(i) = hit(grid(w, h), page.len(), x, y) {
            if self.editing.get() {
                let id = page[i].id.clone();
                let on = {
                    let mut sel = self.selected.borrow_mut();
                    if sel.remove(&id) { false } else { sel.insert(id.clone()) }
                };
                if let Some(c) = self.comp.borrow().as_ref() {
                    for t in self.tiles.borrow().values().flatten().filter(|t| t.id == id) {
                        _ = t.badge.SetBrush(if on { &c.badge_on } else { &c.badge_off });
                    }
                }
                return self.redraw_chrome();
            }
            return self.launch(&page[i]);
        }
        if let Some(p) = hit_dot(w, h, pages(self.shown.borrow().len()), x, y) {
            return self.set_page(p);
        }
        let in_search = (x - w / 2.0).abs() <= SEARCH_W / 2.0 && y >= SEARCH_TOP && y <= SEARCH_TOP + SEARCH_H;
        if !in_search {
            self.close(); // macOS: a click on empty space closes Launchpad
        }
    }

    fn set_editing(&self, on: bool) {
        self.editing.set(on);
        self.selected.borrow_mut().clear();
        if let Some(c) = self.comp.borrow().as_ref() {
            for t in self.tiles.borrow().values().flatten() {
                _ = t.badge.SetBrush(&c.badge_off);
                _ = t.badge.SetIsVisible(on);
            }
        }
    }

    fn press(&self, b: Button) {
        match b {
            Button::Edit => {
                self.set_editing(!self.editing.get());
                self.redraw_chrome();
            }
            Button::Apply => {
                let picked: Vec<String> = self.selected.borrow_mut().drain().collect();
                self.set_hidden(&picked, !self.show_hidden.get());
                self.set_editing(false);
                self.refilter();
            }
            Button::HiddenView => {
                self.show_hidden.set(!self.show_hidden.get());
                self.set_editing(false);
                self.page.set(0);
                self.refilter();
            }
        }
    }

    /// Hides (or shows again) `ids` in Launchpad and saves it.
    fn set_hidden(&self, ids: &[String], hide: bool) {
        if ids.is_empty() {
            return;
        }
        crate::settings::update(|s| {
            s.hidden_apps.retain(|h| !ids.contains(h));
            if hide {
                s.hidden_apps.extend(ids.iter().cloned());
            }
        });
    }

    /// Right click on an app: hide it (or show it again in the hidden view).
    fn quick_menu(&self, x: f32, y: f32) {
        let (w, h) = self.size.get();
        let page = self.page_apps();
        let Some(i) = hit(grid(w, h), page.len(), x, y) else { return };
        let id = page[i].id.clone();
        let hide = !self.show_hidden.get();
        let entries = vec![crate::menu::Entry::item(1, if hide { "Launchpad'de gizle" } else { "Launchpad'de göster" })];
        let mut pt = POINT::default();
        unsafe { _ = GetCursorPos(&mut pt) };
        let lp: &'static Launchpad = unsafe { &*(self as *const Launchpad) };
        crate::menu::show(entries, pt.x, crate::popup::Place::Below(pt.y), move |pick| {
            if pick == 1 {
                lp.set_hidden(&[id], hide);
                lp.refilter();
            }
        });
    }

    fn type_char(&self, c: char) {
        match c {
            '\u{8}' => {
                self.query.borrow_mut().pop();
            }
            c if !c.is_control() => self.query.borrow_mut().push(c),
            _ => return,
        }
        self.page.set(0);
        self.refilter();
    }
}

impl Handler for Launchpad {
    fn handle(&self, _: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APPS => {
                let list = unsafe { Box::from_raw(lp.0 as *mut Vec<App>) };
                if *self.apps.borrow() != *list {
                    *self.apps.borrow_mut() = *list;
                    if self.popup().visible() {
                        self.refilter();
                    } else {
                        // Warm the first page's icons while nobody is looking.
                        let (monitor, dpi) = window::primary_monitor();
                        let s = theme::scale(dpi);
                        let (w, h) = ((monitor.right - monitor.left) as f32 / s, (monitor.bottom - monitor.top) as f32 / s);
                        self.size.set((w, h));
                        self.scale.set(s);
                        self.icon_px.set((grid(w, h).icon * s).round() as u32);
                        let hidden = crate::settings::get().hidden_apps;
                        *self.shown.borrow_mut() = listed(&self.apps.borrow(), "", &hidden, false);
                        self.request(0);
                    }
                }
            }
            WM_ICON => self.on_icon(unsafe { icons::take(lp) }),
            WM_NEIGHBOURS if self.popup().visible() && !self.closing.get() => {
                let p = self.page.get();
                let n = pages(self.shown.borrow().len());
                for q in [p.saturating_sub(1), (p + 1).min(n - 1)] {
                    self.ensure_page(q);
                }
            }
            WM_INPUT => {
                let mut touch = self.touch.borrow_mut();
                match touch.feed(lp) {
                    crate::touchpad::Event::Move(dx_norm) => {
                        drop(touch);
                        let (w, _) = self.size.get();
                        self.drag_by(dx_norm * w * self.scale.get() * 1.5);
                    }
                    crate::touchpad::Event::End => {
                        drop(touch);
                        self.release_drag();
                    }
                    crate::touchpad::Event::None => {}
                }
                return None; // DefWindowProc cleans up the raw input buffer
            }
            WM_TIMER if wp.0 == TIMER_SNAP => {
                unsafe { _ = KillTimer(Some(self.popup().hwnd), TIMER_SNAP) };
                if !self.touch.borrow().active() {
                    self.release_drag();
                }
            }
            WM_TIMER if wp.0 == TIMER_CLOSE => {
                unsafe { _ = KillTimer(Some(self.popup().hwnd), TIMER_CLOSE) };
                self.finish_close();
            }
            WM_MOUSEMOVE => {
                let (x, y) = popup::point(lp, self.popup().scale());
                let (w, h) = self.size.get();
                self.set_hover(hit(grid(w, h), self.page_apps().len(), x, y));
            }
            WM_LBUTTONUP => {
                let (x, y) = popup::point(lp, self.popup().scale());
                self.click(x, y);
            }
            WM_RBUTTONUP => {
                let (x, y) = popup::point(lp, self.popup().scale());
                self.quick_menu(x, y);
            }
            // Two-finger horizontal scroll on a touchpad: the pages follow the fingers.
            WM_MOUSEHWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32;
                self.drag_by(-delta * self.scale.get());
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16;
                let p = self.page.get();
                self.set_page(if delta < 0 { p + 1 } else { p.saturating_sub(1) });
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wp.0 as u32) {
                    self.type_char(c);
                }
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                if vk == VK_ESCAPE.0 {
                    if self.editing.get() {
                        self.press(Button::Edit);
                    } else if self.query.borrow().is_empty() {
                        self.close();
                    } else {
                        self.query.borrow_mut().clear();
                        self.refilter();
                    }
                } else if vk == VK_RIGHT.0 {
                    self.set_page(self.page.get() + 1);
                } else if vk == VK_LEFT.0 {
                    self.set_page(self.page.get().saturating_sub(1));
                } else if vk == VK_RETURN.0
                    && let Some(app) = self.page_apps().first()
                {
                    self.launch(app);
                }
            }
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => self.close(),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}


/// Dock button art: a light tile with a 3 x 3 grid of coloured squares.
pub fn draw_button(dc: &ID2D1DeviceContext, size: f32) -> Result<()> {
    let colours = [
        Rgba(255, 94, 87, 255),
        Rgba(255, 179, 64, 255),
        Rgba(255, 214, 10, 255),
        Rgba(48, 209, 88, 255),
        Rgba(100, 210, 255, 255),
        Rgba(10, 132, 255, 255),
        Rgba(94, 92, 230, 255),
        Rgba(191, 90, 242, 255),
        Rgba(255, 55, 95, 255),
    ];
    unsafe {
        let tile = dc.CreateSolidColorBrush(&Rgba(236, 236, 241, 255).d2d(), None)?;
        let edge = dc.CreateSolidColorBrush(&Rgba(0, 0, 0, 30).d2d(), None)?;
        let inset = size * 0.08;
        let r = D2D_RECT_F { left: inset, top: inset, right: size - inset, bottom: size - inset };
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: size * 0.2, radiusY: size * 0.2 };
        dc.FillRoundedRectangle(&rr, &tile);
        dc.DrawRoundedRectangle(&rr, &edge, 1.0, None);
        let cell = (r.right - r.left) * 0.22;
        let gap = ((r.right - r.left) - 3.0 * cell) / 4.0;
        for (i, c) in colours.iter().enumerate() {
            let (x, y) = (r.left + gap + (i % 3) as f32 * (cell + gap), r.top + gap + (i / 3) as f32 * (cell + gap));
            let b = dc.CreateSolidColorBrush(&c.d2d(), None)?;
            let sq = D2D_RECT_F { left: x, top: y, right: x + cell, bottom: y + cell };
            dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: sq, radiusX: cell * 0.28, radiusY: cell * 0.28 }, &b);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_turns_after_a_fifth() {
        assert_eq!(snap(0, 3, -100.0, 1000.0), 0);
        assert_eq!(snap(0, 3, -250.0, 1000.0), 1);
        assert_eq!(snap(1, 3, 250.0, 1000.0), 0);
        assert_eq!(snap(2, 3, -900.0, 1000.0), 2);
        assert_eq!(snap(0, 3, 900.0, 1000.0), 0);
    }

    fn app(name: &str) -> App {
        App { name: name.into(), id: name.into() }
    }

    #[test]
    fn filter_is_case_insensitive() {
        let apps = [app("Brave"), app("Hesap Makinesi"), app("Visual Studio Code")];
        assert_eq!(filter(&apps, "  "), [0, 1, 2]);
        assert_eq!(filter(&apps, "STUDIO"), [2]);
        assert_eq!(filter(&apps, "makine"), [1]);
        assert!(filter(&apps, "xyz").is_empty());
    }

    #[test]
    fn hidden_apps_move_between_views() {
        let apps = [app("A"), app("B"), app("C")];
        let hidden = vec!["B".to_string()];
        assert_eq!(listed(&apps, "", &hidden, false), [0, 2]);
        assert_eq!(listed(&apps, "", &hidden, true), [1]);
        assert!(listed(&apps, "a", &hidden, true).is_empty(), "search works inside each view");
    }

    #[test]
    fn top_row_buttons() {
        let (w, h) = (1600.0, 1000.0);
        let e = button_rect(w, h, Button::Edit);
        assert_eq!(hit_button(w, h, false, e.left + 5.0, e.top + 5.0), Some(Button::Edit));
        let a = button_rect(w, h, Button::Apply);
        assert_eq!(hit_button(w, h, false, a.left + 5.0, a.top + 5.0), None, "Hide only while editing");
        assert_eq!(hit_button(w, h, true, a.left + 5.0, a.top + 5.0), Some(Button::Apply));
        let hv = button_rect(w, h, Button::HiddenView);
        assert_eq!(hit_button(w, h, false, hv.right - 5.0, hv.top + 5.0), Some(Button::HiddenView));
        let s = (w - SEARCH_W) / 2.0;
        assert!(a.right < s && hv.left > s + SEARCH_W, "pills clear the search field");
    }

    #[test]
    fn pages_and_hits() {
        assert_eq!(pages(0), 1);
        assert_eq!(pages(35), 1);
        assert_eq!(pages(36), 2);
        let g = grid(1600.0, 1000.0);
        assert_eq!(hit(g, 35, g.left + 1.0, g.top + 1.0), Some(0));
        assert_eq!(hit(g, 35, g.left + g.cell_w * 2.5, g.top + g.cell_h * 1.5), Some(9));
        assert_eq!(hit(g, 3, g.left + g.cell_w * 5.0, g.top + 1.0), None, "past the last app");
        assert_eq!(hit(g, 35, g.left - 1.0, g.top + 1.0), None);
        assert!(g.icon <= 88.0 && g.icon > 40.0);
    }

    #[test]
    fn page_dots_hit() {
        let (w, h) = (1600.0, 1000.0);
        assert_eq!(hit_dot(w, h, 3, w / 2.0, dots_y(h)), Some(1));
        assert_eq!(hit_dot(w, h, 3, w / 2.0 - 18.0, dots_y(h)), Some(0));
        assert_eq!(hit_dot(w, h, 1, w / 2.0, dots_y(h)), None, "no dots for one page");
    }

    #[test]
    fn lists_installed_apps() {
        unsafe { _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
        let all = apps();
        assert!(all.len() > 5, "{}", all.len());
        assert!(all.windows(2).all(|p| p[0].name.to_lowercase() <= p[1].name.to_lowercase()));
    }

    #[test]
    fn known_folder_ids_resolve_to_exes() {
        let p = exe_path(r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\magnify.exe").unwrap();
        assert!(p.to_ascii_lowercase().ends_with(r"\system32\magnify.exe"), "{p}");
        assert!(exe_path("steam://rungameid/1").is_none());
        assert!(exe_path("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App").is_none());
        assert!(exe_path(r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\readme.txt").is_none());
    }
}
