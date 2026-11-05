//! Custom context menu in the macOS style: blurred rounded panel, accent
//! highlight, check marks, keyboard (arrows, Enter, Esc). Non-modal: the pick
//! comes back through a callback.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::gfx::{self, Gfx, Label};
use crate::popup::{self, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};

const CLASS: PCWSTR = w!("TaskbarMenu");
const WM_MOUSELEAVE: u32 = 0x02A3;
const ITEM_H: f32 = 26.0;
const SEPARATOR_H: f32 = 9.0;
const PAD: f32 = 5.0;
const TEXT_X: f32 = 28.0;
const MIN_W: f32 = 200.0;
const MAX_LABEL: f32 = 340.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item {
        id: u32,
        label: String,
        checked: bool,
    },
    Separator,
}

impl Entry {
    pub fn item(id: u32, label: impl Into<String>) -> Entry {
        Entry::Item {
            id,
            label: label.into(),
            checked: false,
        }
    }
    fn height(&self) -> f32 {
        match self {
            Entry::Item { .. } => ITEM_H,
            Entry::Separator => SEPARATOR_H,
        }
    }
}

/// Top of each entry and the menu height (DIPs).
pub fn layout(entries: &[Entry]) -> (Vec<f32>, f32) {
    let mut y = PAD;
    let tops = entries
        .iter()
        .map(|e| {
            let t = y;
            y += e.height();
            t
        })
        .collect();
    (tops, y + PAD)
}

/// Item index under `y`; separators are not hits.
pub fn hit(entries: &[Entry], y: f32) -> Option<usize> {
    let (tops, _) = layout(entries);
    entries
        .iter()
        .zip(tops)
        .position(|(e, t)| matches!(e, Entry::Item { .. }) && y >= t && y < t + e.height())
}

/// Next item index from `from` in `dir` (+1/-1), skipping separators, wrapping.
pub fn step(entries: &[Entry], from: Option<usize>, dir: i32) -> Option<usize> {
    let n = entries.len() as i32;
    let mut i = from.map_or(if dir > 0 { -1 } else { n }, |i| i as i32);
    for _ in 0..n {
        i = (i + dir).rem_euclid(n);
        if matches!(entries[i as usize], Entry::Item { .. }) {
            return Some(i as usize);
        }
    }
    None
}

pub struct Fonts {
    pub text: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
    pub cap: f32,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory) -> Result<Self> {
        Ok(Fonts {
            text: gfx::text_format(dwrite, theme::TEXT_FONT, 13.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            icons: gfx::text_format(dwrite, theme::ICON_FONT, 12.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            cap: gfx::cap_height(dwrite, theme::TEXT_FONT, 13.0)?,
        })
    }
}

/// Draws the menu content (DIPs) over the popup's blurred background.
pub fn draw(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    fonts: &Fonts,
    entries: &[Entry],
    w: f32,
    hover: Option<usize>,
    palette: theme::Palette,
) -> Result<()> {
    unsafe {
        let text = dc.CreateSolidColorBrush(&palette.text.d2d(), None)?;
        let on_accent = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 255).d2d(), None)?;
        let (tops, _) = layout(entries);
        let accent = dc.CreateSolidColorBrush(&theme::accent().d2d(), None)?;
        // Hairline separators, lighter than the faint Wi-Fi arcs.
            let line = dc.CreateSolidColorBrush(&Rgba(palette.text.0, palette.text.1, palette.text.2, 34).d2d(), None)?;
        for (i, (e, top)) in entries.iter().zip(tops).enumerate() {
            match e {
                Entry::Separator => {
                    let y = top + SEPARATOR_H / 2.0;
                    dc.FillRectangle(
                        &D2D_RECT_F {
                            left: 12.0,
                            top: y - 0.5,
                            right: w - 12.0,
                            bottom: y + 0.5,
                        },
                        &line,
                    );
                }
                Entry::Item { label, checked, .. } => {
                    let hot = hover == Some(i);
                    if hot {
                        let r = D2D_RECT_F {
                            left: PAD,
                            top,
                            right: w - PAD,
                            bottom: top + ITEM_H,
                        };
                        dc.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: r,
                                radiusX: 5.0,
                                radiusY: 5.0,
                            },
                            &accent,
                        );
                    }
                    let brush = if hot { &on_accent } else { &text };
                    if *checked {
                        Label::new(dwrite, "\u{E73E}", &fonts.icons)?.draw(
                            dc,
                            10.0,
                            (top * 2.0) + ITEM_H,
                            brush,
                        );
                    }
                    let l = Label::fitted(dwrite, label, &fonts.text, w - TEXT_X - 14.0)?;
                    l.draw_text(dc, TEXT_X, top * 2.0 + ITEM_H, fonts.cap, brush);
                }
            }
        }
    }
    Ok(())
}

type Pick = Box<dyn FnOnce(u32)>;

struct Menu {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    text: IDWriteTextFormat,
    icons: IDWriteTextFormat,
    cap: f32,
    entries: RefCell<Vec<Entry>>,
    width: Cell<f32>,
    hover: Cell<Option<usize>>,
    pick: RefCell<Option<Pick>>,
    tracking: Cell<bool>,
}

thread_local! {
    static MENU: OnceCell<&'static Menu> = const { OnceCell::new() };
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let menu: &'static Menu = Box::leak(Box::new(Menu {
        text: gfx::text_format(
            &gfx.dwrite,
            theme::TEXT_FONT,
            13.0,
            DWRITE_FONT_WEIGHT_REGULAR,
        )?,
        icons: gfx::text_format(
            &gfx.dwrite,
            theme::ICON_FONT,
            12.0,
            DWRITE_FONT_WEIGHT_REGULAR,
        )?,
        cap: gfx::cap_height(&gfx.dwrite, theme::TEXT_FONT, 13.0)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        entries: RefCell::new(Vec::new()),
        width: Cell::new(MIN_W),
        hover: Cell::new(None),
        pick: RefCell::new(None),
        tracking: Cell::new(false),
    }));
    _ = menu.popup.set(Popup::create(gfx, CLASS, true, menu)?);
    MENU.with(|m| _ = m.set(menu));
    Ok(())
}

pub fn visible() -> bool {
    MENU.with(|m| m.get().copied())
        .and_then(|m| m.popup.get())
        .is_some_and(Popup::visible)
}

/// Opens the menu at `anchor_x` above or below `place` (screen px). `on_pick`
/// gets the chosen id; it is dropped when the menu closes without a pick.
pub fn show(
    entries: Vec<Entry>,
    anchor_x: i32,
    place: popup::Place,
    on_pick: impl FnOnce(u32) + 'static,
) {
    let Some(m) = MENU.with(|m| m.get().copied()) else {
        return;
    };
    let widest = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Item { label, .. } => Label::new(&m.gfx.dwrite, label, &m.text)
                .ok()
                .map(|l| l.width.min(MAX_LABEL)),
            Entry::Separator => None,
        })
        .fold(0.0, f32::max);
    m.width.set((TEXT_X + widest + 20.0).max(MIN_W).ceil());
    let (_, h) = layout(&entries);
    *m.entries.borrow_mut() = entries;
    *m.pick.borrow_mut() = Some(Box::new(on_pick));
    m.hover.set(None);
    let anchor_y = match place {
        crate::popup::Place::Above(y) | crate::popup::Place::Below(y) => y,
    };
    let (monitor, dpi) = window::monitor_at(anchor_x, anchor_y);
    if let Some(p) = m.popup.get() {
        p.show(
            anchor_x,
            place,
            monitor,
            dpi,
            m.width.get(),
            h,
            |dc| m.draw(dc),
        );
    }
}

impl Menu {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("menu popup")
    }

    fn draw(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let fonts = Fonts {
            text: self.text.clone(),
            icons: self.icons.clone(),
            cap: self.cap,
        };
        draw(
            dc,
            &self.gfx.dwrite,
            &fonts,
            &self.entries.borrow(),
            self.width.get(),
            self.hover.get(),
            theme::system_palette(),
        )
    }

    fn set_hover(&self, h: Option<usize>) {
        if self.hover.replace(h) != h {
            self.popup().redraw(|dc| self.draw(dc));
        }
    }

    fn close(&self, picked: Option<u32>) {
        // Take the pick first: hiding deactivates the menu, and that nested
        // close(None) would otherwise drop the callback before it runs.
        let pick = self.pick.borrow_mut().take();
        self.popup().hide();
        if let (Some(f), Some(id)) = (pick, picked) {
            f(id);
        }
    }

    fn pick_hover(&self) {
        let id = self
            .hover
            .get()
            .and_then(|i| match &self.entries.borrow()[i] {
                Entry::Item { id, .. } => Some(*id),
                Entry::Separator => None,
            });
        if id.is_some() {
            self.close(id);
        }
    }
}

impl Handler for Menu {
    fn handle(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                if !self.tracking.replace(true) {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe { _ = TrackMouseEvent(&mut t) };
                }
                let (_, y) = popup::point(lp, self.popup().scale());
                let h = hit(&self.entries.borrow(), y);
                self.set_hover(h);
            }
            WM_MOUSELEAVE => {
                self.tracking.set(false);
                self.set_hover(None);
            }
            WM_LBUTTONUP | WM_RBUTTONUP => self.pick_hover(),
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                if vk == VK_ESCAPE.0 {
                    self.close(None);
                } else if vk == VK_RETURN.0 || vk == VK_SPACE.0 {
                    self.pick_hover();
                } else if vk == VK_DOWN.0 || vk == VK_UP.0 {
                    let next = step(
                        &self.entries.borrow(),
                        self.hover.get(),
                        if vk == VK_DOWN.0 { 1 } else { -1 },
                    );
                    self.set_hover(next);
                }
            }
            // Clicked elsewhere: the menu lost focus.
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => self.close(None),
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_ACTIVATE as isize)),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            Entry::item(1, "Bir"),
            Entry::Separator,
            Entry::item(2, "İki"),
            Entry::item(3, "Üç"),
        ]
    }

    #[test]
    fn layout_stacks_items_and_separators() {
        let (tops, h) = layout(&sample());
        assert_eq!(
            tops,
            [
                PAD,
                PAD + ITEM_H,
                PAD + ITEM_H + SEPARATOR_H,
                PAD + 2.0 * ITEM_H + SEPARATOR_H
            ]
        );
        assert_eq!(h, PAD * 2.0 + 3.0 * ITEM_H + SEPARATOR_H);
    }

    #[test]
    fn separators_are_not_hit() {
        let e = sample();
        assert_eq!(hit(&e, PAD + 1.0), Some(0));
        assert_eq!(hit(&e, PAD + ITEM_H + 2.0), None);
        assert_eq!(hit(&e, PAD + ITEM_H + SEPARATOR_H + 1.0), Some(2));
        assert_eq!(hit(&e, 0.0), None);
    }

    #[test]
    fn keyboard_steps_skip_separators_and_wrap() {
        let e = sample();
        assert_eq!(step(&e, None, 1), Some(0));
        assert_eq!(step(&e, Some(0), 1), Some(2));
        assert_eq!(step(&e, Some(3), 1), Some(0));
        assert_eq!(step(&e, None, -1), Some(3));
        assert_eq!(step(&e, Some(2), -1), Some(0));
        assert_eq!(step(&[Entry::Separator], None, 1), None);
    }
}
