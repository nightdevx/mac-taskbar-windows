//! Folder stacks: clicking a pinned folder in the dock opens a grid of its
//! newest items above it, like the macOS dock's grid view.

use std::cell::{Cell, OnceCell, RefCell};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};

use crate::gfx::{self, Gfx, Label};
use crate::popup::{self, Place, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};
use crate::icons;

const CLASS: PCWSTR = w!("TaskbarStack");
pub const COLS: usize = 4;
pub const MAX_ITEMS: usize = 12;
const CELL_W: f32 = 84.0;
const CELL_H: f32 = 90.0;
const ICON: f32 = 48.0;
const PAD: f32 = 10.0;
const TITLE_H: f32 = 36.0;
const FOOT_H: f32 = 34.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
}

/// Newest items first (by modification time), hidden and system files left out.
pub fn list(folder: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut items: Vec<(std::time::SystemTime, Entry)> = rd
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if meta.file_attributes() & 0x6 != 0 {
                return None; // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
            }
            let name = e.file_name().to_string_lossy().into_owned();
            Some((meta.modified().ok()?, Entry { path: e.path(), name }))
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0));
    items.into_iter().take(MAX_ITEMS).map(|(_, e)| e).collect()
}

/// Popup size for `n` items (DIPs).
pub fn size(n: usize) -> (f32, f32) {
    let cols = n.clamp(1, COLS) as f32;
    let rows = n.div_ceil(COLS).max(1) as f32;
    (2.0 * PAD + cols * CELL_W, TITLE_H + rows * CELL_H + FOOT_H + PAD)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Item(usize),
    Open,
}

pub fn hit(n: usize, x: f32, y: f32) -> Option<Hit> {
    let (w, h) = size(n);
    if y >= h - FOOT_H - PAD && y < h && x >= 0.0 && x < w {
        return Some(Hit::Open);
    }
    if y < TITLE_H || x < PAD {
        return None;
    }
    let (c, r) = (((x - PAD) / CELL_W) as usize, ((y - TITLE_H) / CELL_H) as usize);
    let i = r * COLS + c;
    (c < COLS && i < n).then_some(Hit::Item(i))
}

fn cell(i: usize) -> D2D_RECT_F {
    let (c, r) = ((i % COLS) as f32, (i / COLS) as f32);
    let left = PAD + c * CELL_W;
    let top = TITLE_H + r * CELL_H;
    D2D_RECT_F { left: left + 2.0, top, right: left + CELL_W - 2.0, bottom: top + CELL_H - 4.0 }
}

struct Stack {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    text: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    small: IDWriteTextFormat,
    cap: f32,
    folder: RefCell<PathBuf>,
    entries: RefCell<Vec<Entry>>,
    icons: RefCell<Vec<icons::Icon>>,
    hover: Cell<Option<Hit>>,
}

thread_local! {
    static STACK: OnceCell<&'static Stack> = const { OnceCell::new() };
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let small = gfx::text_format(&gfx.dwrite, theme::TEXT_FONT, 11.0, DWRITE_FONT_WEIGHT_REGULAR)?;
    unsafe { small.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)? };
    let s: &'static Stack = Box::leak(Box::new(Stack {
        text: gfx::text_format(&gfx.dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_REGULAR)?,
        bold: gfx::text_format(&gfx.dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
        small,
        cap: gfx::cap_height(&gfx.dwrite, theme::TEXT_FONT, 13.0)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        folder: RefCell::new(PathBuf::new()),
        entries: RefCell::new(Vec::new()),
        icons: RefCell::new(Vec::new()),
        hover: Cell::new(None),
    }));
    _ = s.popup.set(Popup::create(gfx, CLASS, true, s)?);
    STACK.with(|c| _ = c.set(s));
    Ok(())
}

/// Opens the grid for `folder` next to the dock icon at `anchor` (screen px).
pub fn show(folder: &str, anchor: i32, place: Place) {
    let Some(s) = STACK.with(|c| c.get().copied()) else { return };
    let (monitor, dpi) = window::primary_monitor();
    let px = (ICON * theme::scale(dpi)).round() as u32;
    let entries = list(Path::new(folder));
    // ponytail: icons load on the UI thread (12 at most); use the icon worker if it shows.
    *s.icons.borrow_mut() = entries.iter().map(|e| icons::load(&e.path.to_string_lossy(), px)).collect();
    *s.entries.borrow_mut() = entries;
    *s.folder.borrow_mut() = PathBuf::from(folder);
    s.hover.set(None);
    let (w, h) = size(s.entries.borrow().len());
    s.popup().show(anchor, place, monitor, dpi, w, h, |dc| s.draw(dc));
}

impl Stack {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("stack popup")
    }

    fn draw(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let palette = theme::system_palette();
        let entries = self.entries.borrow();
        let (w, h) = size(entries.len());
        let dwrite = &self.gfx.dwrite;
        unsafe {
            let text = dc.CreateSolidColorBrush(&palette.text.d2d(), None)?;
            let hover = dc.CreateSolidColorBrush(&palette.hover.d2d(), None)?;
            let accent = dc.CreateSolidColorBrush(&theme::accent().d2d(), None)?;
            let line = dc.CreateSolidColorBrush(&Rgba(palette.text.0, palette.text.1, palette.text.2, 34).d2d(), None)?;
            let name = self.folder.borrow().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let title = Label::fitted(dwrite, &name, &self.bold, w - 2.0 * PAD)?;
            title.draw_text(dc, (w - title.width) / 2.0, TITLE_H, self.cap, &text);
            if entries.is_empty() {
                let l = Label::new(dwrite, "Klasör boş", &self.text)?;
                l.draw_text(dc, (w - l.width) / 2.0, TITLE_H * 2.0 + CELL_H, self.cap, &text);
            }
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0 * self.popup().scale(),
                dpiY: 96.0 * self.popup().scale(),
                ..Default::default()
            };
            for (i, (e, icon)) in entries.iter().zip(self.icons.borrow().iter()).enumerate() {
                let r = cell(i);
                if self.hover.get() == Some(Hit::Item(i)) {
                    dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 8.0, radiusY: 8.0 }, &hover);
                }
                let left = (r.left + r.right - ICON) / 2.0;
                let top = r.top + 8.0;
                if !icon.pixels.is_empty() {
                    let bmp = dc.CreateBitmap(D2D_SIZE_U { width: icon.size, height: icon.size }, Some(icon.pixels.as_ptr() as *const _), icon.size * 4, &props)?;
                    dc.DrawBitmap(&bmp, Some(&D2D_RECT_F { left, top, right: left + ICON, bottom: top + ICON }), 1.0, D2D1_INTERPOLATION_MODE_LINEAR, None, None);
                }
                // Two centred lines, like Finder labels.
                let wide: Vec<u16> = e.name.encode_utf16().collect();
                let layout = dwrite.CreateTextLayout(&wide, &self.small, r.right - r.left - 6.0, 30.0)?;
                layout.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
                let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                layout.SetTrimming(&trim, &dwrite.CreateEllipsisTrimmingSign(&self.small)?)?;
                dc.DrawTextLayout(windows_numerics::Vector2 { X: r.left + 3.0, Y: top + ICON + 6.0 }, &layout, &text, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            let foot_top = h - FOOT_H - PAD;
            dc.FillRectangle(&D2D_RECT_F { left: PAD, top: foot_top, right: w - PAD, bottom: foot_top + 1.0 }, &line);
            let foot = D2D_RECT_F { left: PAD / 2.0, top: foot_top + 4.0, right: w - PAD / 2.0, bottom: h - PAD / 2.0 };
            if self.hover.get() == Some(Hit::Open) {
                dc.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: foot, radiusX: 6.0, radiusY: 6.0 }, &hover);
            }
            let open = Label::new(dwrite, "Gezgin'de aç  \u{203A}", &self.text)?;
            open.draw_text(dc, (w - open.width) / 2.0, foot.top + foot.bottom, self.cap, &accent);
        }
        Ok(())
    }

    fn launch(&self, target: &Path) {
        self.popup().hide();
        unsafe {
            ShellExecuteW(None, w!("open"), &HSTRING::from(target.as_os_str()), None, None, SW_SHOWNORMAL);
        }
    }
}

impl Handler for Stack {
    fn handle(&self, _: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = popup::point(lp, self.popup().scale());
                let h = hit(self.entries.borrow().len(), x, y);
                if self.hover.replace(h) != h {
                    self.popup().redraw(|dc| self.draw(dc));
                }
            }
            WM_LBUTTONUP => {
                let (x, y) = popup::point(lp, self.popup().scale());
                match hit(self.entries.borrow().len(), x, y) {
                    Some(Hit::Item(i)) => {
                        let p = self.entries.borrow()[i].path.clone();
                        self.launch(&p);
                    }
                    Some(Hit::Open) => {
                        let p = self.folder.borrow().clone();
                        self.launch(&p);
                    }
                    None => {}
                }
            }
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => self.popup().hide(),
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => self.popup().hide(),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_size_and_hits() {
        assert_eq!(size(2).0, 2.0 * PAD + 2.0 * CELL_W, "narrow for few items");
        assert_eq!(size(9).0, 2.0 * PAD + COLS as f32 * CELL_W);
        let (_, h) = size(9);
        assert_eq!(hit(9, PAD + 1.0, TITLE_H + 1.0), Some(Hit::Item(0)));
        assert_eq!(hit(9, PAD + CELL_W + 1.0, TITLE_H + CELL_H + 1.0), Some(Hit::Item(5)));
        assert_eq!(hit(9, PAD + 3.0 * CELL_W + 1.0, TITLE_H + 2.0 * CELL_H + 1.0), None, "past the last item");
        assert_eq!(hit(9, 40.0, h - 10.0), Some(Hit::Open));
        assert_eq!(hit(9, 40.0, 5.0), None, "title");
    }

    #[test]
    fn lists_newest_first_without_hidden_files() {
        let dir = std::env::temp_dir().join(format!("taskbar-stack-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.txt"), "a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.txt"), "b").unwrap();
        let names: Vec<String> = list(&dir).into_iter().map(|e| e.name).collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(names, ["new.txt", "old.txt"]);
    }
}
