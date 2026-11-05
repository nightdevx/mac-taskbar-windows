//! `--snapshot <png>`: draws the bars into a PNG without opening a window,
//! for visual checks. The blurred backdrop is simulated with a flat colour.

use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{HSTRING, Interface, Result};
use windows_numerics::Matrix3x2;

use crate::menubar::{self, Fonts, State};
use crate::theme::{self, Rgba};
use crate::{clock, gfx, status};

const SCALE: f32 = 1.5;
const WIDTH: f32 = 1280.0;
const WALLPAPER: Rgba = Rgba(96, 112, 138, 255);

pub fn run(path: &str) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let dwrite = gfx::dwrite()?;
    let fonts = Fonts::new(&dwrite)?;
    let bar_h = theme::MENUBAR_HEIGHT;
    let gap = 12.0;
    // Typical state in both themes, then edge states: long app name, low battery,
    // muted, no internet, offline.
    let normal = status::Status {
        battery: Some(status::Battery { percent: 72, plugged: false }),
        volume: Some(status::Volume { level: 0.6, muted: false }),
        net: status::Net::Wifi(3),
    };
    let edge = status::Status {
        battery: Some(status::Battery { percent: 7, plugged: false }),
        volume: Some(status::Volume { level: 0.6, muted: true }),
        net: status::Net::NoInternet,
    };
    let long = "Microsoft Visual Studio Code - Insiders Edition, a name long enough to be cut off before the icons. ".repeat(3);
    let rows = [
        (theme::DARK, "Gezgin", normal),
        (theme::LIGHT, "Gezgin", normal),
        (theme::DARK, long.as_str(), edge),
        (theme::LIGHT, "Ayarlar", status::Status { battery: Some(status::Battery { percent: 100, plugged: true }), net: status::Net::Offline, ..edge }),
    ];
    let height = rows.len() as f32 * (bar_h + gap) + gap;

    render_png(path, WIDTH, height, |dc| unsafe {
        let wall = dc.CreateSolidColorBrush(&WALLPAPER.d2d(), None)?;
        dc.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: WIDTH, bottom: height }, &wall);
        for (i, (palette, app, st)) in rows.into_iter().enumerate() {
            let y = gap + i as f32 * (bar_h + gap);
            dc.SetTransform(&Matrix3x2::translation(0.0, y));
            let fill = dc.CreateSolidColorBrush(&palette.bar.d2d(), None)?;
            dc.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: WIDTH, bottom: bar_h }, &fill);
            let st = State { app_name: app.into(), clock: clock::text(&clock::now()), status: status::icons(&st), palette };
            menubar::draw(dc, &dwrite, &fonts, &st, WIDTH, bar_h)?;
        }
        Ok(())
    })
}

/// Renders `draw` (DIP coordinates) at `SCALE` into a PNG file.
fn render_png(path: &str, w: f32, h: f32, draw: impl FnOnce(&ID2D1DeviceContext) -> Result<()>) -> Result<()> {
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            (w * SCALE).ceil() as u32,
            (h * SCALE).ceil() as u32,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0 * SCALE,
            dpiY: 96.0 * SCALE,
            ..Default::default()
        };
        let dc: ID2D1DeviceContext = factory.CreateWicBitmapRenderTarget(&bitmap, &props)?.cast()?;
        dc.BeginDraw();
        let drawn = draw(&dc);
        dc.EndDraw(None, None)?;
        drawn?;

        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.expect("CreateNewFrame returned no frame");
        frame.Initialize(None)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()
    }
}
