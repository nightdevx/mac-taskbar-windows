//! Rounded, blurred popup window with one drawn surface: shared by the dock's
//! context menu and its hover previews. Created once, then shown and hidden.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::Foundation::TimeSpan;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionDrawingSurface, CompositionRoundedRectangleGeometry, ContainerVisual, SpriteVisual,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1DeviceContext;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result};
use windows_numerics::{Vector2, Vector3};

use crate::gfx::Gfx;
use crate::theme;
use crate::window::{self, Backdrop, Handler};

pub const RADIUS: f32 = 10.0;

/// Where a popup sits relative to its anchor (screen px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// Bottom edge at this y (dock menus and previews).
    Above(i32),
    /// Top edge at this y (menu bar panels).
    Below(i32),
    /// Left edge at this x, centred on the anchor y (left dock).
    RightOf(i32),
    /// Right edge at this x, centred on the anchor y (right dock).
    LeftOf(i32),
}

impl Place {
    /// A screen point on the anchor, for picking the monitor.
    pub fn point(self, anchor: i32) -> (i32, i32) {
        match self {
            Place::Above(y) | Place::Below(y) => (anchor, y),
            Place::RightOf(x) | Place::LeftOf(x) => (x, anchor),
        }
    }
}
const APPEAR_MS: i64 = 120;

struct Comp {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    backdrop: Backdrop,
    shape: CompositionRoundedRectangleGeometry,
    content: SpriteVisual,
    surface: CompositionDrawingSurface,
    /// Above the drawn content: callers add their own visuals here.
    layer: ContainerVisual,
}

pub struct Popup {
    pub hwnd: HWND,
    gfx: Rc<Gfx>,
    comp: RefCell<Option<Comp>>,
    /// Size in px and scale of the last show.
    size: Cell<(i32, i32)>,
    scale: Cell<f32>,
    activate: bool,
}

impl Popup {
    /// `activate`: the menu takes focus (so it closes on an outside click);
    /// previews never do.
    pub fn create<T: Handler + 'static>(
        gfx: Rc<Gfx>,
        class: PCWSTR,
        activate: bool,
        handler: &'static T,
    ) -> Result<Popup> {
        let mut ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP;
        if !activate {
            ex |= WS_EX_NOACTIVATE;
        }
        let hwnd = window::create_with(class, ex, WS_POPUP, handler)?;
        let c = &gfx.compositor;
        let target = gfx.target(hwnd)?;
        let root = c.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let frame = c.CreateContainerVisual()?;
        frame.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        let shape = c.CreateRoundedRectangleGeometry()?;
        frame.SetClip(&c.CreateGeometricClipWithGeometry(&shape)?)?;
        let backdrop = Backdrop::new(c, hwnd)?;
        frame.Children()?.InsertAtTop(&backdrop.blur)?;
        frame.Children()?.InsertAtTop(&backdrop.tint_visual)?;
        let surface = gfx.surface(1, 1)?;
        let content = c.CreateSpriteVisual()?;
        content.SetBrush(&c.CreateSurfaceBrushWithSurface(&surface)?)?;
        frame.Children()?.InsertAtTop(&content)?;
        let layer = c.CreateContainerVisual()?;
        layer.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        frame.Children()?.InsertAtTop(&layer)?;
        root.Children()?.InsertAtTop(&frame)?;
        target.SetRoot(&root)?;
        let comp = Comp {
            _target: target,
            root,
            backdrop,
            shape,
            content,
            surface,
            layer,
        };
        Ok(Popup {
            hwnd,
            gfx,
            comp: RefCell::new(Some(comp)),
            size: Cell::new((0, 0)),
            scale: Cell::new(1.0),
            activate,
        })
    }

    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd) }.as_bool()
    }

    pub fn scale(&self) -> f32 {
        self.scale.get()
    }

    /// Shows a `w` x `h` DIP popup centred on `anchor_x` (on the anchor y for
    /// `RightOf`/`LeftOf`), on the `place` side, kept inside `monitor`.
    pub fn show(
        &self,
        anchor_x: i32,
        place: Place,
        monitor: RECT,
        dpi: u32,
        w: f32,
        h: f32,
        draw: impl Fn(&ID2D1DeviceContext) -> Result<()>,
    ) {
        let s = theme::scale(dpi);
        let (wp, hp) = ((w * s).round() as i32, (h * s).round() as i32);
        let centred_x = (anchor_x - wp / 2).clamp(
            monitor.left + 4,
            (monitor.right - wp - 4).max(monitor.left + 4),
        );
        let centred_y = (anchor_x - hp / 2).clamp(monitor.top + 4, (monitor.bottom - hp - 4).max(monitor.top + 4));
        let (x, y) = match place {
            Place::Above(bottom) => (centred_x, (bottom - hp).max(monitor.top + 4)),
            Place::Below(top) => (centred_x, top.min(monitor.bottom - hp - 4)),
            Place::RightOf(left) => (left.min(monitor.right - wp - 4), centred_y),
            Place::LeftOf(right) => ((right - wp).max(monitor.left + 4), centred_y),
        };
        let pivot = match place {
            Place::Below(_) => 0,
            Place::Above(_) => hp,
            Place::RightOf(_) | Place::LeftOf(_) => hp / 2,
        };
        self.show_rect(RECT { left: x, top: y, right: x + wp, bottom: y + hp }, dpi, RADIUS, pivot, draw);
    }

    /// Shows at exactly `rc` (screen px) with corner `radius` (DIPs); the
    /// appear animation grows from `pivot_y` px down the window.
    pub fn show_rect(&self, rc: RECT, dpi: u32, radius: f32, pivot_y: i32, draw: impl Fn(&ID2D1DeviceContext) -> Result<()>) {
        let s = theme::scale(dpi);
        let (x, y, wp, hp) = (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top);
        let was_visible = self.visible();
        self.scale.set(s);
        self.size.set((wp, hp));
        unsafe { _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, wp, hp, SWP_NOACTIVATE) };
        if let Some(c) = self.comp.borrow().as_ref() {
            c.backdrop.apply(theme::system_palette().popup);
            _ = c.shape.SetSize(Vector2 {
                X: wp as f32,
                Y: hp as f32,
            });
            _ = c.shape.SetCornerRadius(Vector2 {
                X: radius * s,
                Y: radius * s,
            });
            _ = c.content.SetSize(Vector2 {
                X: wp as f32,
                Y: hp as f32,
            });
            _ = self.gfx.resize(&c.surface, wp as u32, hp as u32);
        }
        self.redraw(draw);
        if !was_visible {
            self.appear(wp, pivot_y);
            unsafe {
                _ = ShowWindow(
                    self.hwnd,
                    if self.activate {
                        SW_SHOW
                    } else {
                        SW_SHOWNOACTIVATE
                    },
                );
                if self.activate {
                    _ = SetForegroundWindow(self.hwnd);
                }
            }
        }
    }

    /// Fade in while growing from 96 % around the anchored edge's centre (macOS menus).
    fn appear(&self, wp: i32, pivot_y: i32) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        if !theme::animations() {
            return;
        }
        let comp_ = &self.gfx.compositor;
        let run = || -> Result<()> {
            let duration = TimeSpan {
                Duration: APPEAR_MS * 10_000,
            };
            let fade = comp_.CreateScalarKeyFrameAnimation()?;
            fade.InsertKeyFrame(0.0, 0.0)?;
            fade.InsertKeyFrame(1.0, 1.0)?;
            fade.SetDuration(duration)?;
            let grow = comp_.CreateVector3KeyFrameAnimation()?;
            grow.InsertKeyFrame(
                0.0,
                Vector3 {
                    X: 0.96,
                    Y: 0.96,
                    Z: 1.0,
                },
            )?;
            grow.InsertKeyFrame(
                1.0,
                Vector3 {
                    X: 1.0,
                    Y: 1.0,
                    Z: 1.0,
                },
            )?;
            grow.SetDuration(duration)?;
            c.root.SetCenterPoint(Vector3 {
                X: wp as f32 / 2.0,
                Y: pivot_y as f32,
                Z: 0.0,
            })?;
            c.root.StartAnimation(&HSTRING::from("Opacity"), &fade)?;
            c.root.StartAnimation(&HSTRING::from("Scale"), &grow)
        };
        _ = run();
    }

    pub fn redraw(&self, draw: impl Fn(&ID2D1DeviceContext) -> Result<()>) {
        if let Some(c) = self.comp.borrow().as_ref()
            && let Err(e) = self.gfx.draw(&c.surface, self.scale.get(), draw)
        {
            crate::log::error(&format!("popup draw: {e}"));
        }
    }

    pub fn layer(&self) -> Option<ContainerVisual> {
        self.comp.borrow().as_ref().map(|c| c.layer.clone())
    }

    /// Fades the whole popup out over `ms` (hide it after that).
    pub fn fade_out(&self, ms: i64) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
        let run = || -> Result<()> {
            let fade = self.gfx.compositor.CreateScalarKeyFrameAnimation()?;
            fade.InsertKeyFrame(0.0, 1.0)?;
            fade.InsertKeyFrame(1.0, 0.0)?;
            fade.SetDuration(TimeSpan { Duration: ms * 10_000 })?;
            c.root.StartAnimation(&HSTRING::from("Opacity"), &fade)
        };
        _ = run();
    }

    /// Hides and frees the drawing surface (a full-screen one is many MB).
    pub fn hide_and_release(&self) {
        self.hide();
        if let Some(c) = self.comp.borrow().as_ref() {
            _ = self.gfx.resize(&c.surface, 1, 1);
        }
    }

    /// Tint over the blur for this popup (Launchpad is darker than menus).
    pub fn set_tint(&self, color: theme::Rgba) {
        if let Some(c) = self.comp.borrow().as_ref() {
            c.backdrop.apply(color);
        }
    }

    pub fn hide(&self) {
        unsafe { _ = ShowWindow(self.hwnd, SW_HIDE) };
    }

    /// Pointer inside this popup's window rect.
    pub fn has_pointer(&self) -> bool {
        self.visible() && pointer_in(self.hwnd)
    }
}

pub fn pointer_in(hwnd: HWND) -> bool {
    let mut pt = POINT::default();
    let mut rc = RECT::default();
    unsafe {
        _ = GetCursorPos(&mut pt);
        _ = GetWindowRect(hwnd, &mut rc);
    }
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

/// Client point from a mouse message, in DIPs.
pub fn point(lp: windows::Win32::Foundation::LPARAM, scale: f32) -> (f32, f32) {
    (
        (lp.0 & 0xFFFF) as i16 as f32 / scale,
        ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / scale,
    )
}
