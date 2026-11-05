//! `--snapshot <png>`: draws the bars into a PNG without opening a window,
//! for visual checks. The blurred backdrop is simulated with a flat colour.

use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::{HSTRING, Interface, Result};
use windows_numerics::Matrix3x2;

use crate::menubar::{self, Fonts, State};
use crate::theme::{self, Rgba};
use crate::{clock, dock, gfx, icons, magnify, prefs, status};

const SCALE: f32 = 1.5;
const WIDTH: f32 = 1280.0;
const WALLPAPER: Rgba = Rgba(96, 112, 138, 255);

pub fn run(path: &str) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let dwrite = gfx::dwrite()?;
    prefs_png(&path.replace(".png", "-prefs.png"), &dwrite)?;
    menu_png(&path.replace(".png", "-menu.png"), &dwrite)?;
    panels_png(&path.replace(".png", "-panels.png"), &dwrite)?;
    launchpad_png(&path.replace(".png", "-launchpad.png"), &dwrite)?;
    let fonts = Fonts::new(&dwrite)?;
    let bar_h = theme::MENUBAR_HEIGHT;
    let gap = 12.0;
    // Typical state in both themes, then edge states: long app name, low battery,
    // muted, no internet, offline.
    let normal = status::Status {
        battery: Some(status::Battery {
            percent: 72,
            plugged: false,
        }),
        volume: Some(status::Volume {
            level: 0.6,
            muted: false,
        }),
        net: status::Net::Wifi(3),
    };
    let edge = status::Status {
        battery: Some(status::Battery {
            percent: 7,
            plugged: false,
        }),
        volume: Some(status::Volume {
            level: 0.6,
            muted: true,
        }),
        net: status::Net::NoInternet { wifi: false },
    };
    let long = "Microsoft Visual Studio Code - Insiders Edition, a name long enough to be cut off before the icons. ".repeat(3);
    // Live entries: default size at rest (dark), default size magnified with the
    // pointer on the third icon (light), largest size at rest (dark).
    let default = crate::settings::Settings::default();
    let mut dock_items: Vec<(icons::Icon, bool)> = dock::entries(&crate::settings::get())
        .iter()
        .map(|e| {
            (
                icons::load(
                    &e.source,
                    (crate::settings::DOCK_ICON_MAX * 2.0 * SCALE).round() as u32,
                ),
                !e.windows.is_empty(),
            )
        })
        .collect();
    // Right of the separator: pinned folders and the Trash, as the live dock.
    let apps = dock_items.len();
    let px = (crate::settings::DOCK_ICON_MAX * 2.0 * SCALE).round() as u32;
    for f in crate::settings::folders(&crate::settings::get()) {
        dock_items.push((icons::load(&f, px), false));
    }
    dock_items.push((icons::load(r"shell:RecycleBinFolder", px), false));
    let n = dock_items.len();
    let base = dock::Geometry {
        icon: default.dock_icon,
        sep: Some(apps),
    };
    let pointer = base.item_x(2.min(n.saturating_sub(1))) + base.icon * 0.6;
    let docks = [
        (base, None, theme::DARK),
        (base, Some(pointer), theme::LIGHT),
        (
            dock::Geometry {
                icon: crate::settings::DOCK_ICON_MAX,
                sep: Some(apps),
            },
            None,
            theme::DARK,
        ),
    ];
    let rows = [
        (theme::DARK, "Gezgin", normal),
        (theme::LIGHT, "Gezgin", normal),
        (theme::DARK, long.as_str(), edge),
        (
            theme::LIGHT,
            "Ayarlar",
            status::Status {
                battery: Some(status::Battery {
                    percent: 100,
                    plugged: true,
                }),
                net: status::Net::Offline,
                ..edge
            },
        ),
        (
            theme::LIGHT,
            "Not Defteri",
            status::Status {
                battery: Some(status::Battery {
                    percent: 9,
                    plugged: false,
                }),
                net: status::Net::NoInternet { wifi: true },
                ..normal
            },
        ),
    ];
    let dock_top = rows.len() as f32 * (bar_h + gap) + gap;
    let grow = |g: dock::Geometry| g.icon * (default.dock_magnify - 1.0);
    let height = dock_top
        + docks
            .iter()
            .map(|(g, ..)| g.height() + grow(*g) + 2.0 * gap)
            .sum::<f32>();

    render_png(path, WIDTH, height, |dc| unsafe {
        let wall = dc.CreateSolidColorBrush(&WALLPAPER.d2d(), None)?;
        dc.FillRectangle(
            &D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: WIDTH,
                bottom: height,
            },
            &wall,
        );
        for (i, (palette, app, st)) in rows.into_iter().enumerate() {
            let y = gap + i as f32 * (bar_h + gap);
            dc.SetTransform(&Matrix3x2::translation(0.0, y));
            let fill = dc.CreateSolidColorBrush(&palette.bar.d2d(), None)?;
            dc.FillRectangle(
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: WIDTH,
                    bottom: bar_h,
                },
                &fill,
            );
            let st = State {
                app_name: app.into(),
                clock: clock::text(&clock::now()),
                status: status::icons(&st, true),
                palette,
            };
            menubar::draw(dc, &dwrite, &fonts, &st, WIDTH, bar_h)?;
        }
        let mut y = dock_top + gap;
        for (g, pointer, palette) in docks {
            let l = magnify::layout(g, n, pointer, default.dock_magnify);
            let w = g.width(n);
            dc.SetTransform(&Matrix3x2::translation(
                ((WIDTH - w) / 2.0).round(),
                y + grow(g),
            ));
            draw_dock(dc, g, &l, &dock_items, palette)?;
            y += g.height() + grow(g) + 2.0 * gap;
        }
        Ok(())
    })
}

/// Menu bar panels with sample data (no live audio or Wi-Fi calls): calendar,
/// Wi-Fi, battery, volume; light on top, dark below.
fn panels_png(path: &str, dwrite: &windows::Win32::Graphics::DirectWrite::IDWriteFactory) -> Result<()> {
    use crate::panels::{self, Row};
    let fonts = panels::Fonts::new(dwrite)?;
    let nets = vec![
        crate::wifi::Network { ssid: "Ev_5G".into(), quality: 92, secured: true, profile: Some("Ev_5G".into()), connected: true },
        crate::wifi::Network { ssid: "Kafe Misafir".into(), quality: 55, secured: false, profile: None, connected: false },
        crate::wifi::Network { ssid: "Komşu".into(), quality: 18, secured: true, profile: None, connected: false },
    ];
    let sets: Vec<Vec<Row>> = vec![
        panels::calendar_rows(),
        panels::wifi_rows(Some(true), &crate::wifi::Scan::Networks(nets), None),
        panels::battery_rows(Some(status::Battery { percent: 64, plugged: false }), Some(3 * 3600 + 20 * 60), Some(crate::power::Mode::Balanced)),
        panels::volume_rows(Some(status::Volume { level: 0.45, muted: false })),
    ];
    let heights: Vec<f32> = sets.iter().map(|r| panels::layout(r).1).collect();
    let tallest = heights.iter().cloned().fold(0.0, f32::max);
    let gap = 20.0;
    let w = sets.len() as f32 * (panels::WIDTH + gap) + gap;
    let h = 2.0 * (tallest + gap) + gap;
    let now = clock::now();
    render_png(path, w, h, |dc| unsafe {
        let wall = dc.CreateSolidColorBrush(&WALLPAPER.d2d(), None)?;
        dc.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: h }, &wall);
        for (row_i, palette) in [theme::LIGHT, theme::DARK].into_iter().enumerate() {
            for (i, rows) in sets.iter().enumerate() {
                let (x, y) = (gap + i as f32 * (panels::WIDTH + gap), gap + row_i as f32 * (tallest + gap));
                dc.SetTransform(&Matrix3x2::translation(x, y));
                let bg = dc.CreateSolidColorBrush(&palette.popup.opaque().d2d(), None)?;
                dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: 0.0, top: 0.0, right: panels::WIDTH, bottom: heights[i] }, radiusX: 10.0, radiusY: 10.0 },
                    &bg,
                );
                let view = panels::View {
                    rows,
                    hover: if i == 1 { Some(2) } else { None },
                    month: (now.wYear as i32, now.wMonth as u32),
                    today: (now.wYear as i32, now.wMonth as u32, now.wDay as u32),
                    palette,
                };
                panels::draw(dc, dwrite, &fonts, &view)?;
            }
        }
        Ok(())
    })
}

/// Launchpad's first page with the installed apps, over a flat wallpaper.
fn launchpad_png(path: &str, dwrite: &windows::Win32::Graphics::DirectWrite::IDWriteFactory) -> Result<()> {
    use crate::launchpad;
    let fonts = launchpad::Fonts::new(dwrite)?;
    let (w, h) = (1280.0, 800.0);
    let apps = launchpad::apps();
    let page: Vec<launchpad::App> = apps.into_iter().take(launchpad::PER_PAGE).collect();
    let px = (launchpad::grid(w, h).icon * SCALE).round() as u32;
    let icons = page
        .iter()
        .map(|a| {
            let src = a.icon_source();
            (src.clone(), icons::load(&src, px))
        })
        .collect();
    let selected: std::collections::HashSet<String> = page.iter().skip(2).take(2).map(|a| a.id.clone()).collect();
    let view = launchpad::View {
        size: (w, h),
        query: "",
        apps: &page,
        icons: &icons,
        loading: false,
        page: 0,
        pages: 3,
        hover: Some(8),
        scale: SCALE,
        editing: true,
        selected: &selected,
        show_hidden: false,
        hidden_count: 4,
    };
    render_png(path, w, h, |dc| unsafe {
        let wall = dc.CreateSolidColorBrush(&Rgba(52, 60, 78, 255).d2d(), None)?;
        dc.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: h }, &wall);
        launchpad::draw(dc, dwrite, &fonts, &view)
    })
}

/// Dock context menu in both themes: a checked window, the second entry hovered.
fn menu_png(
    path: &str,
    dwrite: &windows::Win32::Graphics::DirectWrite::IDWriteFactory,
) -> Result<()> {
    use crate::menu::{self, Entry};
    let fonts = menu::Fonts::new(dwrite)?;
    let entries = vec![
        Entry::Item {
            id: 1,
            label: "Windows'ta Ubuntu tarzı görev çubuğu - Visual Studio Code".into(),
            checked: true,
        },
        Entry::item(2, "taskbar - Gezgin"),
        Entry::Separator,
        Entry::item(3, "Visual Studio Code"),
        Entry::item(4, "Dock'tan kaldır"),
        Entry::item(5, "Dosya konumunu aç"),
        Entry::Separator,
        Entry::item(6, "Tüm pencereleri kapat"),
    ];
    let (_, h) = menu::layout(&entries);
    let w = 300.0;
    let gap = 24.0;
    render_png(path, 2.0 * w + 3.0 * gap, h + 2.0 * gap, |dc| unsafe {
        let wall = dc.CreateSolidColorBrush(&WALLPAPER.d2d(), None)?;
        dc.FillRectangle(
            &D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: 2.0 * w + 3.0 * gap,
                bottom: h + 2.0 * gap,
            },
            &wall,
        );
        for (i, palette) in [theme::LIGHT, theme::DARK].into_iter().enumerate() {
            dc.SetTransform(&Matrix3x2::translation(gap + i as f32 * (w + gap), gap));
            let bg = dc.CreateSolidColorBrush(&palette.popup.opaque().d2d(), None)?;
            dc.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: w,
                        bottom: h,
                    },
                    radiusX: 10.0,
                    radiusY: 10.0,
                },
                &bg,
            );
            menu::draw(dc, dwrite, &fonts, &entries, w, Some(1), palette)?;
        }
        Ok(())
    })
}

/// Settings window in light and dark, side by side, with the size slider focused.
fn prefs_png(
    path: &str,
    dwrite: &windows::Win32::Graphics::DirectWrite::IDWriteFactory,
) -> Result<()> {
    let fonts = prefs::Fonts::new(dwrite)?;
    let l = prefs::layout();
    let gap = 24.0;
    render_png(
        path,
        2.0 * prefs::WIDTH + 3.0 * gap,
        l.height + 2.0 * gap,
        |dc| unsafe {
            let wall = dc.CreateSolidColorBrush(&WALLPAPER.d2d(), None)?;
            dc.FillRectangle(
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: 2.0 * prefs::WIDTH + 3.0 * gap,
                    bottom: l.height + 2.0 * gap,
                },
                &wall,
            );
            for (i, dark) in [false, true].into_iter().enumerate() {
                let x = gap + i as f32 * (prefs::WIDTH + gap);
                dc.SetTransform(&Matrix3x2::translation(x, gap));
                let clip = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: prefs::WIDTH,
                    bottom: l.height,
                };
                dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
                let view = prefs::View {
                    settings: crate::settings::Settings::default(),
                    focus: Some(prefs::CONTROLS[0]),
                    active: !dark,
                    palette: prefs::palette(dark),
                };
                let r = prefs::draw(dc, dwrite, &fonts, &l, &view);
                dc.PopAxisAlignedClip();
                r?;
            }
            Ok(())
        },
    )
}

/// Same geometry as the live dock (dock::Geometry), drawn flat for review.
fn draw_dock(
    dc: &ID2D1DeviceContext,
    g: dock::Geometry,
    l: &magnify::Layout,
    items: &[(icons::Icon, bool)],
    palette: theme::Palette,
) -> Result<()> {
    unsafe {
        let h = g.height();
        let fill = dc.CreateSolidColorBrush(&palette.bar.d2d(), None)?;
        let r = g.radius();
        dc.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: l.left,
                    top: 0.0,
                    right: l.right,
                    bottom: h,
                },
                radiusX: r,
                radiusY: r,
            },
            &fill,
        );
        let dot = dc.CreateSolidColorBrush(&palette.text.d2d(), None)?;
        if let Some(i) = g.sep.filter(|i| g.sep_before(*i, items.len())) {
            let line = dc.CreateSolidColorBrush(&Rgba(palette.text.0, palette.text.1, palette.text.2, 70).d2d(), None)?;
            let x = l.items[i].0 - g.sep_w() / 2.0 - g.gap() / 2.0;
            let bottom = g.pad() + g.icon;
            dc.FillRectangle(&D2D_RECT_F { left: x, top: bottom - g.icon * 0.7, right: x + 1.0, bottom }, &line);
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0 * SCALE,
            dpiY: 96.0 * SCALE,
            ..Default::default()
        };
        for ((icon, running), (x, k)) in items.iter().zip(&l.items) {
            let (x, side) = (*x, g.icon * k);
            let top = g.pad() + g.icon - side; // grows upward from the icon's bottom
            if !icon.pixels.is_empty() {
                let bmp = dc.CreateBitmap(
                    D2D_SIZE_U {
                        width: icon.size,
                        height: icon.size,
                    },
                    Some(icon.pixels.as_ptr() as *const _),
                    icon.size * 4,
                    &props,
                )?;
                dc.DrawBitmap(
                    &bmp,
                    Some(&D2D_RECT_F {
                        left: x,
                        top,
                        right: x + side,
                        bottom: top + side,
                    }),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
            if *running {
                let d = theme::DOCK_DOT / 2.0;
                let cy = top + side + g.pad_bottom() / 2.0;
                dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: windows_numerics::Vector2 {
                            X: x + side / 2.0,
                            Y: cy,
                        },
                        radiusX: d,
                        radiusY: d,
                    },
                    &dot,
                );
            }
        }
    }
    Ok(())
}

/// Renders `draw` (DIP coordinates) at `SCALE` into a PNG file.
fn render_png(
    path: &str,
    w: f32,
    h: f32,
    draw: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
) -> Result<()> {
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            (w * SCALE).ceil() as u32,
            (h * SCALE).ceil() as u32,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0 * SCALE,
            dpiY: 96.0 * SCALE,
            ..Default::default()
        };
        let dc: ID2D1DeviceContext = factory
            .CreateWicBitmapRenderTarget(&bitmap, &props)?
            .cast()?;
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
