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
const APPEAR_MS: i64 = 120;

struct Comp {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    backdrop: Backdrop,
    shape: CompositionRoundedRectangleGeometry,
    content: SpriteVisual,
    surface: CompositionDrawingSurface,
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
        root.Children()?.InsertAtTop(&frame)?;
        target.SetRoot(&root)?;
        let comp = Comp {
            _target: target,
            root,
            backdrop,
            shape,
            content,
            surface,
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

    /// Shows a `w` x `h` DIP popup centred on `anchor_x` with its bottom at
    /// `anchor_bottom` (screen px), kept inside `monitor`.
    pub fn show(
        &self,
        anchor_x: i32,
        anchor_bottom: i32,
        monitor: RECT,
        dpi: u32,
        w: f32,
        h: f32,
        draw: impl Fn(&ID2D1DeviceContext) -> Result<()>,
    ) {
        let s = theme::scale(dpi);
        let (wp, hp) = ((w * s).round() as i32, (h * s).round() as i32);
        let x = (anchor_x - wp / 2).clamp(
            monitor.left + 4,
            (monitor.right - wp - 4).max(monitor.left + 4),
        );
        let y = (anchor_bottom - hp).max(monitor.top + 4);
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
                X: RADIUS * s,
                Y: RADIUS * s,
            });
            _ = c.content.SetSize(Vector2 {
                X: wp as f32,
                Y: hp as f32,
            });
            _ = self.gfx.resize(&c.surface, wp as u32, hp as u32);
        }
        self.redraw(draw);
        if !was_visible {
            self.appear(wp, hp);
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

    /// Fade in while growing from 96 % around the bottom centre (macOS menus).
    fn appear(&self, wp: i32, hp: i32) {
        let comp = self.comp.borrow();
        let Some(c) = comp.as_ref() else { return };
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
                Y: hp as f32,
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
