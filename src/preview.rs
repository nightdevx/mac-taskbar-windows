//! Hover previews above the dock: live DWM thumbnails of an app's windows with
//! their titles and a close button, like the Windows taskbar. Apps without
//! windows get just their name in a bubble, like the macOS dock label.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::gfx::{self, Gfx, Label};
use crate::popup::{self, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};

const CLASS: PCWSTR = w!("TaskbarPreview");
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HIDE: usize = 1;
const HIDE_DELAY_MS: u32 = 300;

const OUTER: f32 = 6.0;
const CARD_PAD: f32 = 8.0;
const TITLE_H: f32 = 22.0;
const GAP: f32 = 4.0;
const THUMB_W: f32 = 220.0;
const THUMB_H: f32 = 130.0;
const CARD_MIN_W: f32 = 150.0;
const CLOSE: f32 = 18.0;
const LABEL_PAD_X: f32 = 12.0;
const LABEL_H: f32 = 28.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Card {
    pub left: f32,
    pub width: f32,
    pub thumb: D2D_RECT_F,
}

/// Cards side by side; thumbnails keep the window's aspect ratio inside
/// THUMB_W x THUMB_H, shrunk together when the row would exceed `max_w`.
/// Returns the cards and the popup size.
pub fn layout(sources: &[(f32, f32)], max_w: f32) -> (Vec<Card>, f32, f32) {
    let fit = |(w, h): (f32, f32), k: f32| {
        let (w, h) = (w.max(1.0), h.max(1.0));
        let s = (THUMB_W * k / w).min(THUMB_H * k / h);
        (w * s, h * s)
    };
    let row = |k: f32| -> f32 {
        let cards: f32 = sources
            .iter()
            .map(|s| fit(*s, k).0.max(CARD_MIN_W * k) + 2.0 * CARD_PAD)
            .sum();
        cards + GAP * sources.len().saturating_sub(1) as f32 + 2.0 * OUTER
    };
    // Gaps, margins and card padding stay fixed; only thumbnails shrink.
    let fixed = GAP * sources.len().saturating_sub(1) as f32
        + 2.0 * OUTER
        + 2.0 * CARD_PAD * sources.len() as f32;
    let k = ((max_w - fixed) / (row(1.0) - fixed)).min(1.0);
    let thumb_h = THUMB_H * k;
    let mut x = OUTER;
    let cards = sources
        .iter()
        .map(|s| {
            let (tw, th) = fit(*s, k);
            let width = tw.max(CARD_MIN_W * k) + 2.0 * CARD_PAD;
            let top = OUTER + CARD_PAD + TITLE_H + (thumb_h - th) / 2.0;
            let left = x + (width - tw) / 2.0;
            let card = Card {
                left: x,
                width,
                thumb: D2D_RECT_F {
                    left,
                    top,
                    right: left + tw,
                    bottom: top + th,
                },
            };
            x += width + GAP;
            card
        })
        .collect();
    (
        cards,
        row(k),
        OUTER * 2.0 + CARD_PAD * 2.0 + TITLE_H + thumb_h,
    )
}

fn close_rect(c: &Card) -> D2D_RECT_F {
    let r = c.left + c.width - CARD_PAD + 2.0;
    let t = OUTER + CARD_PAD - 1.0;
    D2D_RECT_F {
        left: r - CLOSE,
        top: t,
        right: r,
        bottom: t + CLOSE,
    }
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

struct Shown {
    hwnd: HWND,
    title: String,
    thumb: isize,
}

struct Preview {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    text: IDWriteTextFormat,
    icons: IDWriteTextFormat,
    cap: f32,
    dock: Cell<HWND>,
    shown: RefCell<Vec<Shown>>,
    cards: RefCell<Vec<Card>>,
    /// Name bubble when the app has no windows.
    label: RefCell<Option<String>>,
    size: Cell<(f32, f32)>,
    anchor: Cell<(i32, popup::Place)>,
    hover: Cell<Option<(usize, bool)>>,
    tracking: Cell<bool>,
}

thread_local! {
    static PREVIEW: OnceCell<&'static Preview> = const { OnceCell::new() };
}

fn get() -> Option<&'static Preview> {
    PREVIEW.with(|p| p.get().copied())
}

pub fn init(gfx: Rc<Gfx>, dock: HWND) -> Result<()> {
    let p: &'static Preview = Box::leak(Box::new(Preview {
        text: gfx::text_format(
            &gfx.dwrite,
            theme::TEXT_FONT,
            12.0,
            DWRITE_FONT_WEIGHT_REGULAR,
        )?,
        icons: gfx::text_format(
            &gfx.dwrite,
            theme::ICON_FONT,
            10.0,
            DWRITE_FONT_WEIGHT_REGULAR,
        )?,
        cap: gfx::cap_height(&gfx.dwrite, theme::TEXT_FONT, 12.0)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        dock: Cell::new(dock),
        shown: RefCell::new(Vec::new()),
        cards: RefCell::new(Vec::new()),
        label: RefCell::new(None),
        size: Cell::new((0.0, 0.0)),
        anchor: Cell::new((0, popup::Place::Above(0))),
        hover: Cell::new(None),
        tracking: Cell::new(false),
    }));
    _ = p.popup.set(Popup::create(gfx, CLASS, false, p)?);
    PREVIEW.with(|c| _ = c.set(p));
    Ok(())
}

/// Shows previews of `windows`, or the name bubble when there are none,
/// centred on `anchor_x` with the bottom at `anchor_bottom` (screen px).
pub fn show(windows: &[HWND], name: &str, anchor: i32, place: popup::Place) {
    let Some(p) = get() else { return };
    p.cancel_hide();
    p.release();
    let popup = p.popup();
    let mut shown = Vec::new();
    for &hwnd in windows
        .iter()
        .filter(|h| unsafe { IsWindow(Some(**h)) }.as_bool())
    {
        let Ok(thumb) = (unsafe { DwmRegisterThumbnail(popup.hwnd, hwnd) }) else {
            continue;
        };
        let mut buf = [0u16; 256];
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        shown.push(Shown {
            hwnd,
            title: String::from_utf16_lossy(&buf[..n]),
            thumb,
        });
    }
    *p.label.borrow_mut() = shown.is_empty().then(|| name.to_string());
    *p.shown.borrow_mut() = shown;
    p.anchor.set((anchor, place));
    p.hover.set(None);
    p.relayout();
}

pub fn hide() {
    if let Some(p) = get() {
        p.popup().hide();
        p.release();
    }
}

/// Hides after a short delay unless the pointer comes back to the dock or the preview.
pub fn hide_soon() {
    if let Some(p) = get()
        && p.popup().visible()
    {
        unsafe { SetTimer(Some(p.popup().hwnd), TIMER_HIDE, HIDE_DELAY_MS, None) };
    }
}

pub fn visible() -> bool {
    get().is_some_and(|p| p.popup().visible())
}

/// A window closed while its preview may be showing.
pub fn forget(hwnd: HWND) {
    let Some(p) = get() else { return };
    let had = p.shown.borrow().iter().any(|s| s.hwnd == hwnd);
    if had {
        p.remove(hwnd);
    }
}

impl Preview {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("preview popup")
    }

    fn cancel_hide(&self) {
        unsafe { _ = KillTimer(Some(self.popup().hwnd), TIMER_HIDE) };
    }

    fn release(&self) {
        for s in self.shown.borrow_mut().drain(..) {
            unsafe { _ = DwmUnregisterThumbnail(s.thumb) };
        }
    }

    fn remove(&self, hwnd: HWND) {
        let mut shown = self.shown.borrow_mut();
        if let Some(i) = shown.iter().position(|s| s.hwnd == hwnd) {
            unsafe { _ = DwmUnregisterThumbnail(shown.remove(i).thumb) };
        }
        let empty = shown.is_empty();
        drop(shown);
        if empty {
            self.popup().hide();
        } else {
            self.relayout();
        }
    }

    fn relayout(&self) {
        let (monitor, dpi) = window::primary_monitor();
        let s = theme::scale(dpi);
        let max_w = (monitor.right - monitor.left) as f32 / s - 16.0;
        let (w, h) = match self.label.borrow().as_ref() {
            Some(name) => {
                let l = Label::new(&self.gfx.dwrite, name, &self.text)
                    .map(|l| l.width)
                    .unwrap_or(80.0);
                self.cards.borrow_mut().clear();
                ((l + 2.0 * LABEL_PAD_X).ceil(), LABEL_H)
            }
            None => {
                let sizes: Vec<(f32, f32)> = self
                    .shown
                    .borrow()
                    .iter()
                    .map(|sh| {
                        unsafe { DwmQueryThumbnailSourceSize(sh.thumb) }
                            .map_or((16.0, 9.0), |z| (z.cx as f32, z.cy as f32))
                    })
                    .collect();
                let (cards, w, h) = layout(&sizes, max_w);
                *self.cards.borrow_mut() = cards;
                (w, h)
            }
        };
        self.size.set((w, h));
        let (anchor, place) = self.anchor.get();
        self.popup().show(anchor, place, monitor, dpi, w, h, |dc| self.draw(dc));
        // Thumbnails are composited by DWM on top of our content.
        for (sh, card) in self.shown.borrow().iter().zip(self.cards.borrow().iter()) {
            let r = card.thumb;
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: (r.left * s) as i32,
                    top: (r.top * s) as i32,
                    right: (r.right * s) as i32,
                    bottom: (r.bottom * s) as i32,
                },
                opacity: 255,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
                ..Default::default()
            };
            unsafe { _ = DwmUpdateThumbnailProperties(sh.thumb, &props) };
        }
    }

    fn draw(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let palette = theme::system_palette();
        unsafe {
            let text = dc.CreateSolidColorBrush(&palette.text.d2d(), None)?;
            if let Some(name) = self.label.borrow().as_ref() {
                Label::new(&self.gfx.dwrite, name, &self.text)?.draw_text(
                    dc,
                    LABEL_PAD_X,
                    LABEL_H,
                    self.cap,
                    &text,
                );
                return Ok(());
            }
            let hover = dc.CreateSolidColorBrush(&palette.hover.d2d(), None)?;
            let close_bg = dc.CreateSolidColorBrush(&Rgba(196, 43, 28, 255).d2d(), None)?;
            let white = dc.CreateSolidColorBrush(&Rgba(255, 255, 255, 255).d2d(), None)?;
            let (_, h) = self.size.get();
            for (i, (card, sh)) in self
                .cards
                .borrow()
                .iter()
                .zip(self.shown.borrow().iter())
                .enumerate()
            {
                let hot = self.hover.get().filter(|(c, _)| *c == i);
                let box_ = D2D_RECT_F {
                    left: card.left,
                    top: OUTER,
                    right: card.left + card.width,
                    bottom: h - OUTER,
                };
                if hot.is_some() {
                    dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: box_,
                            radiusX: 7.0,
                            radiusY: 7.0,
                        },
                        &hover,
                    );
                }
                let title_w =
                    card.width - 2.0 * CARD_PAD - if hot.is_some() { CLOSE + 4.0 } else { 0.0 };
                Label::fitted(&self.gfx.dwrite, &sh.title, &self.text, title_w)?.draw_text(
                    dc,
                    card.left + CARD_PAD,
                    (OUTER + CARD_PAD) * 2.0 + TITLE_H - 6.0,
                    self.cap,
                    &text,
                );
                if let Some((_, on_close)) = hot {
                    let r = close_rect(card);
                    if on_close {
                        dc.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: r,
                                radiusX: 4.0,
                                radiusY: 4.0,
                            },
                            &close_bg,
                        );
                    }
                    let glyph = Label::new(&self.gfx.dwrite, "\u{E711}", &self.icons)?;
                    glyph.draw(
                        dc,
                        r.left + (CLOSE - glyph.width) / 2.0,
                        r.top * 2.0 + CLOSE,
                        if on_close { &white } else { &text },
                    );
                }
            }
        }
        Ok(())
    }

    fn hit(&self, x: f32, y: f32) -> Option<(usize, bool)> {
        let (_, h) = self.size.get();
        self.cards.borrow().iter().enumerate().find_map(|(i, c)| {
            let box_ = D2D_RECT_F {
                left: c.left,
                top: OUTER,
                right: c.left + c.width,
                bottom: h - OUTER,
            };
            inside(&box_, x, y).then(|| (i, inside(&close_rect(c), x, y)))
        })
    }

    fn click(&self, x: f32, y: f32) {
        let Some((i, on_close)) = self.hit(x, y) else {
            return;
        };
        let Some(hwnd) = self.shown.borrow().get(i).map(|s| s.hwnd) else {
            return;
        };
        if on_close {
            unsafe { _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            self.remove(hwnd);
        } else {
            hide();
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    _ = ShowWindow(hwnd, SW_RESTORE);
                }
                _ = SetForegroundWindow(hwnd);
            }
        }
    }
}

impl Handler for Preview {
    fn handle(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                self.cancel_hide();
                if !self.tracking.replace(true) {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe { _ = TrackMouseEvent(&mut t) };
                }
                let (x, y) = popup::point(lp, self.popup().scale());
                let h = self.hit(x, y);
                if self.hover.replace(h) != h {
                    self.popup().redraw(|dc| self.draw(dc));
                }
            }
            WM_MOUSELEAVE => {
                self.tracking.set(false);
                if self.hover.take().is_some() {
                    self.popup().redraw(|dc| self.draw(dc));
                }
                hide_soon();
            }
            WM_LBUTTONUP => {
                let (x, y) = popup::point(lp, self.popup().scale());
                self.click(x, y);
            }
            WM_TIMER if wp.0 == TIMER_HIDE => {
                self.cancel_hide();
                if !popup::pointer_in(self.dock.get()) && !self.popup().has_pointer() {
                    hide();
                }
            }
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_keep_aspect_and_cards_do_not_overlap() {
        let (cards, w, h) = layout(&[(1920.0, 1080.0), (800.0, 1200.0), (300.0, 200.0)], 2000.0);
        let wide = cards[0].thumb;
        assert!(((wide.right - wide.left) / (wide.bottom - wide.top) - 16.0 / 9.0).abs() < 0.01);
        assert!(wide.right - wide.left <= THUMB_W + 0.01);
        let tall = cards[1].thumb;
        assert!(
            (tall.bottom - tall.top - THUMB_H).abs() < 0.01,
            "tall windows fill the height"
        );
        assert!(
            cards[1].width >= CARD_MIN_W + 2.0 * CARD_PAD - 0.01,
            "narrow thumbs keep a readable card"
        );
        for pair in cards.windows(2) {
            assert!(pair[1].left >= pair[0].left + pair[0].width);
        }
        assert!((w - (cards[2].left + cards[2].width + OUTER)).abs() < 0.01);
        assert!(h > THUMB_H);
    }

    #[test]
    fn row_shrinks_to_fit_the_screen() {
        let many = vec![(1920.0, 1080.0); 10];
        let (cards, w, _) = layout(&many, 1200.0);
        assert!(w <= 1200.0 + 0.01, "{w}");
        assert_eq!(cards.len(), 10);
    }
}
