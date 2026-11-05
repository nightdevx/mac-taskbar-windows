//! Graphics: one D3D11/D2D device for drawing, DirectWrite for text and the
//! Windows.UI.Composition compositor that owns visuals and animations.
//! Drawing happens only when content changes; DWM composes and animates.

use std::cell::RefCell;

use windows::Foundation::Size;
use windows::Graphics::DirectX::{DirectXAlphaMode, DirectXPixelFormat};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{CompositionDrawingSurface, CompositionGraphicsDevice, Compositor};
use windows::Win32::Foundation::{HMODULE, HWND, POINT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Composition::{
    ICompositionDrawingSurfaceInterop, ICompositionGraphicsDeviceInterop, ICompositorDesktopInterop, ICompositorInterop,
};
use windows::core::{Interface, PCWSTR, Result, w};
use windows_numerics::Matrix3x2;

pub struct Gfx {
    pub compositor: Compositor,
    pub dwrite: IDWriteFactory,
    d2d_factory: ID2D1Factory1,
    graphics: CompositionGraphicsDevice,
    /// Kept so a lost device can be replaced in place.
    d2d_device: RefCell<ID2D1Device>,
}

/// WARP (CPU rasteriser) on purpose: measured 7.8 MB private / 9 threads against
/// 41 MB / 23 threads for the GPU driver. We only draw when content changes;
/// DWM still composes and animates the visuals on the GPU.
fn d3d_device() -> Result<ID3D11Device> {
    let mut dev = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        )?;
    }
    Ok(dev.expect("D3D11CreateDevice returned no device"))
}

fn d2d_device(factory: &ID2D1Factory1) -> Result<ID2D1Device> {
    unsafe { factory.CreateDevice(&d3d_device()?.cast::<IDXGIDevice>()?) }
}

impl Gfx {
    /// Needs a DispatcherQueue on the calling thread (see main.rs).
    pub fn new() -> Result<Self> {
        let d2d_factory: ID2D1Factory1 = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let dwrite = dwrite()?;
        let device = d2d_device(&d2d_factory)?;
        let compositor = Compositor::new()?;
        let graphics = unsafe { compositor.cast::<ICompositorInterop>()?.CreateGraphicsDevice(&device)? };
        Ok(Self { compositor, dwrite, d2d_factory, graphics, d2d_device: RefCell::new(device) })
    }

    pub fn target(&self, hwnd: HWND) -> Result<DesktopWindowTarget> {
        unsafe { self.compositor.cast::<ICompositorDesktopInterop>()?.CreateDesktopWindowTarget(hwnd, true) }
    }

    pub fn surface(&self, w: u32, h: u32) -> Result<CompositionDrawingSurface> {
        self.graphics.CreateDrawingSurface(
            Size { Width: w.max(1) as f32, Height: h.max(1) as f32 },
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            DirectXAlphaMode::Premultiplied,
        )
    }

    pub fn resize(&self, surface: &CompositionDrawingSurface, w: u32, h: u32) -> Result<()> {
        let size = SIZE { cx: w.max(1) as i32, cy: h.max(1) as i32 };
        unsafe { surface.cast::<ICompositionDrawingSurfaceInterop>()?.Resize(size) }
    }

    /// Replaces the D2D device after a GPU reset or driver update.
    /// Composition then drops surface contents, so callers redraw everything.
    pub fn recreate_device(&self) -> Result<()> {
        let device = d2d_device(&self.d2d_factory)?;
        unsafe { self.graphics.cast::<ICompositionGraphicsDeviceInterop>()?.SetRenderingDevice(&device)? };
        *self.d2d_device.borrow_mut() = device;
        Ok(())
    }

    /// Clears the surface and runs `f` with DIP coordinates (scale = dpi / 96).
    /// On device loss it recreates the device and tries once more.
    pub fn draw(
        &self,
        surface: &CompositionDrawingSurface,
        scale: f32,
        f: impl Fn(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        match self.draw_once(surface, scale, &f) {
            Err(e) if is_device_lost(&e) => {
                crate::log::error(&format!("device lost while drawing: {e}"));
                self.recreate_device()?;
                self.draw_once(surface, scale, &f)
            }
            r => r,
        }
    }

    fn draw_once(
        &self,
        surface: &CompositionDrawingSurface,
        scale: f32,
        f: &impl Fn(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        let interop: ICompositionDrawingSurfaceInterop = surface.cast()?;
        let mut offset = POINT::default();
        let dc: ID2D1DeviceContext = unsafe { interop.BeginDraw(None, &mut offset)? };
        let result = unsafe {
            dc.SetDpi(96.0 * scale, 96.0 * scale);
            dc.SetTransform(&Matrix3x2::translation(offset.x as f32 / scale, offset.y as f32 / scale));
            dc.Clear(Some(&D2D1_COLOR_F::default()));
            f(&dc)
        };
        let end = unsafe { interop.EndDraw() };
        result.and(end)
    }
}

pub fn dwrite() -> Result<IDWriteFactory> {
    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
}

pub fn text_format(dwrite: &IDWriteFactory, family: &str, size: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
    let family: Vec<u16> = family.encode_utf16().chain([0]).collect();
    unsafe {
        let fmt = dwrite.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!(""), // empty = invariant locale; null is rejected
        )?;
        fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(fmt)
    }
}

fn is_device_lost(e: &windows::core::Error) -> bool {
    use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
    [DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, windows::Win32::Foundation::D2DERR_RECREATE_TARGET].contains(&e.code())
}

/// Text laid out once, measured, then drawn.
pub struct Label {
    pub layout: IDWriteTextLayout,
    pub width: f32,
    pub height: f32,
}

impl Label {
    pub fn new(dwrite: &IDWriteFactory, text: &str, fmt: &IDWriteTextFormat) -> Result<Self> {
        Self::fitted(dwrite, text, fmt, f32::MAX)
    }

    /// Text cut with an ellipsis ("Visual Studio Co…") when wider than `max_w`.
    pub fn fitted(dwrite: &IDWriteFactory, text: &str, fmt: &IDWriteTextFormat, max_w: f32) -> Result<Self> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = dwrite.CreateTextLayout(&wide, fmt, max_w.max(0.0), f32::MAX)?;
            if max_w < f32::MAX {
                let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                layout.SetTrimming(&trimming, &dwrite.CreateEllipsisTrimmingSign(fmt)?)?;
            }
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            Ok(Self { layout, width: m.widthIncludingTrailingWhitespace.min(max_w), height: m.height })
        }
    }

    /// Draws with the left edge at `x`, vertically centred in a band `[0, band_h]`.
    pub fn draw(&self, dc: &ID2D1DeviceContext, x: f32, band_h: f32, brush: &ID2D1SolidColorBrush) {
        let y = ((band_h - self.height) / 2.0).round();
        unsafe {
            dc.DrawTextLayout(
                windows_numerics::Vector2 { X: x, Y: y },
                &self.layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            )
        };
    }
}
