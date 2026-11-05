//! Launchpad: every installed app in a full-screen grid over the blurred
//! desktop, with search and pages, like macOS Launchpad. Opened from the dock
//! (the "show applications" button, as on Ubuntu).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

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
use windows::core::{HSTRING, PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::gfx::{self, Gfx, Label};
use crate::icons::{self, Loader};
use crate::popup::{self, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};

const CLASS: PCWSTR = w!("TaskbarLaunchpad");
const WM_ICON: u32 = WM_APP + 1;
/// The app list, read on a worker thread (enumerating the Apps folder takes ~1 s).
const WM_APPS: u32 = WM_APP + 2;
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
            let msg = if v.loading { "Uygulamalar yükleniyor…" } else { "Sonuç yok" };
            let l = Label::new(dwrite, msg, &f.text)?;
            l.draw_text(dc, (w - l.width) / 2.0, h, f.cap, &dim);
        }
        for (i, app) in apps.iter().enumerate() {
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


struct Launchpad {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    label: IDWriteTextFormat,
    text: IDWriteTextFormat,
    icons_font: IDWriteTextFormat,
    cap: f32,
    loader: OnceCell<Loader>,
    apps: RefCell<Vec<App>>,
    shown: RefCell<Vec<usize>>,
    query: RefCell<String>,
    page: Cell<usize>,
    hover: Cell<Option<usize>>,
    icons: RefCell<HashMap<String, icons::Icon>>,
    requested: RefCell<HashSet<String>>,
    size: Cell<(f32, f32)>,
    icon_px: Cell<u32>,
}

thread_local! {
    static LP: OnceCell<&'static Launchpad> = const { OnceCell::new() };
}

fn get() -> Option<&'static Launchpad> {
    LP.with(|l| l.get().copied())
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let label = gfx::text_format(&gfx.dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_REGULAR)?;
    unsafe { label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)? };
    let lp: &'static Launchpad = Box::leak(Box::new(Launchpad {
        label,
        text: gfx::text_format(&gfx.dwrite, theme::TEXT_FONT, 14.0, DWRITE_FONT_WEIGHT_REGULAR)?,
        icons_font: gfx::text_format(&gfx.dwrite, theme::ICON_FONT, 14.0, DWRITE_FONT_WEIGHT_REGULAR)?,
        cap: gfx::cap_height(&gfx.dwrite, theme::TEXT_FONT, 14.0)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        loader: OnceCell::new(),
        apps: RefCell::new(Vec::new()),
        shown: RefCell::new(Vec::new()),
        query: RefCell::new(String::new()),
        page: Cell::new(0),
        hover: Cell::new(None),
        icons: RefCell::new(HashMap::new()),
        requested: RefCell::new(HashSet::new()),
        size: Cell::new((0.0, 0.0)),
        icon_px: Cell::new(64),
    }));
    let popup = Popup::create(gfx, CLASS, true, lp)?;
    _ = lp.loader.set(Loader::start(popup.hwnd, WM_ICON));
    _ = lp.popup.set(popup);
    LP.with(|l| _ = l.set(lp));
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
    let (monitor, dpi) = window::primary_monitor();
    let s = theme::scale(dpi);
    let (w, h) = ((monitor.right - monitor.left) as f32 / s, (monitor.bottom - monitor.top) as f32 / s);
    lp.size.set((w, h));
    lp.icon_px.set((grid(w, h).icon * s).round() as u32);
    // Last known list at once; a fresh one follows from the worker.
    refresh_apps(lp.popup().hwnd);
    lp.query.borrow_mut().clear();
    lp.page.set(0);
    lp.hover.set(None);
    lp.refilter();
    let pivot = (monitor.bottom - monitor.top) / 2;
    lp.popup().set_tint(Rgba(18, 18, 22, 120));
    lp.popup().show_rect(monitor, dpi, 0.0, pivot, |dc| lp.draw(dc));
    lp.popup().set_tint(Rgba(18, 18, 22, 120));
}

impl Launchpad {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("launchpad popup")
    }

    fn close(&self) {
        self.popup().hide_and_release();
        // Free the icons: Launchpad is rare, memory is not.
        self.icons.borrow_mut().clear();
        self.requested.borrow_mut().clear();
    }

    fn refilter(&self) {
        let shown = filter(&self.apps.borrow(), &self.query.borrow());
        *self.shown.borrow_mut() = shown;
        self.page.set(self.page.get().min(pages(self.shown.borrow().len()) - 1));
        self.request_page();
    }

    fn page_apps(&self) -> Vec<App> {
        let apps = self.apps.borrow();
        self.shown.borrow().iter().skip(self.page.get() * PER_PAGE).take(PER_PAGE).map(|i| apps[*i].clone()).collect()
    }

    /// Asks the worker for this page's icons.
    fn request_page(&self) {
        let Some(loader) = self.loader.get() else { return };
        for app in self.page_apps() {
            let src = app.icon_source();
            if self.requested.borrow_mut().insert(src.clone()) {
                loader.request(&src, self.icon_px.get());
            }
        }
    }

    fn redraw(&self) {
        self.popup().redraw(|dc| self.draw(dc));
    }

    fn set_page(&self, page: usize) {
        let n = pages(self.shown.borrow().len());
        let page = page.min(n - 1);
        if page != self.page.get() {
            self.page.set(page);
            self.hover.set(None);
            self.request_page();
            self.redraw();
        }
    }

    fn launch(&self, app: &App) {
        self.close();
        unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(app.source()), None, None, SW_SHOWNORMAL) };
    }

    fn draw(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let fonts = Fonts { label: self.label.clone(), text: self.text.clone(), icons: self.icons_font.clone(), cap: self.cap };
        let icons = self.icons.borrow();
        let page = self.page_apps();
        let view = View {
            size: self.size.get(),
            query: &self.query.borrow(),
            apps: &page,
            icons: &icons,
            loading: self.apps.borrow().is_empty(),
            page: self.page.get(),
            pages: pages(self.shown.borrow().len()),
            hover: self.hover.get(),
            scale: self.popup().scale(),
        };
        draw(dc, &self.gfx.dwrite, &fonts, &view)
    }

    fn click(&self, x: f32, y: f32) {
        let (w, h) = self.size.get();
        let page = self.page_apps();
        if let Some(i) = hit(grid(w, h), page.len(), x, y) {
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

    fn type_char(&self, c: char) {
        match c {
            '\u{8}' => {
                self.query.borrow_mut().pop();
            }
            c if !c.is_control() => self.query.borrow_mut().push(c),
            _ => return,
        }
        self.page.set(0);
        self.hover.set(None);
        self.refilter();
        self.redraw();
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
                        self.redraw();
                    }
                }
            }
            WM_ICON => {
                let icon = unsafe { icons::take(lp) };
                if self.popup().visible() {
                    self.icons.borrow_mut().insert(icon.source.clone(), *icon);
                    self.redraw();
                }
            }
            WM_MOUSEMOVE => {
                let (x, y) = popup::point(lp, self.popup().scale());
                let (w, h) = self.size.get();
                let h_ = hit(grid(w, h), self.page_apps().len(), x, y);
                if self.hover.replace(h_) != h_ {
                    self.redraw();
                }
            }
            WM_LBUTTONUP => {
                let (x, y) = popup::point(lp, self.popup().scale());
                self.click(x, y);
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
                    if self.query.borrow().is_empty() {
                        self.close();
                    } else {
                        self.query.borrow_mut().clear();
                        self.refilter();
                        self.redraw();
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
