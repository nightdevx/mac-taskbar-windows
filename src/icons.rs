//! App icons from the shell, extracted on a worker thread so the UI never waits
//! on disk or on the shell namespace. Results come back as `WM_ICON` messages.

use std::sync::mpsc::{Sender, channel};

use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHDefExtractIconW, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, HICON, ICONINFO, PostMessageW,
};
use windows::core::HSTRING;

/// A square icon, BGRA with premultiplied alpha. Empty `pixels` means extraction failed.
pub struct Icon {
    pub source: String,
    pub size: u32,
    pub pixels: Vec<u8>,
}

/// Top-down 32-bit pixels of a `size` x `size` bitmap; the bitmap is deleted.
fn take_pixels(bmp: HBITMAP, size: u32) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (size * size * 4) as usize];
    unsafe {
        let dc = GetDC(None);
        let rows = GetDIBits(
            dc,
            bmp,
            0,
            size,
            Some(px.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        _ = DeleteObject(HGDIOBJ(bmp.0));
        (rows == size as i32).then_some(px)
    }
}

/// Icon resource of an exe, scaled by the shell to `size`. Unlike the item
/// image factory it never puts a small icon inside a grey "document" frame.
fn exe_icon(path: &str, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut hicon = HICON::default();
        SHDefExtractIconW(&HSTRING::from(path), 0, 0, Some(&mut hicon), None, size)
            .ok()
            .ok()?;
        let mut info = ICONINFO::default();
        let got = GetIconInfo(hicon, &mut info);
        _ = DestroyIcon(hicon);
        got.ok()?;
        let color = take_pixels(info.hbmColor, size);
        let mask = take_pixels(info.hbmMask, size);
        let mut px = color?;
        // Old icons carry transparency only in the AND mask (white = transparent).
        if px.chunks_exact(4).all(|p| p[3] == 0)
            && let Some(mask) = mask
        {
            for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        Some(px)
    }
}

/// Shell icon for a parsing name: an exe or .lnk path, or `shell:AppsFolder\<AUMID>`.
/// Needs COM on the calling thread.
pub fn load(source: &str, size: u32) -> Icon {
    let from_exe = || {
        source
            .to_ascii_lowercase()
            .ends_with(".exe")
            .then(|| exe_icon(source, size))
            .flatten()
    };
    let from_shell = || unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(&HSTRING::from(source), None)
            .and_then(|f| {
                f.GetImage(
                    SIZE {
                        cx: size as i32,
                        cy: size as i32,
                    },
                    // SCALEUP: stretch small icons instead of framing them in a grey document.
                    SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK | windows::Win32::UI::Shell::SIIGBF_SCALEUP,
                )
            })
            .ok()
            .and_then(|bmp| take_pixels(bmp, size))
    };
    let mut pixels = from_exe().or_else(from_shell).unwrap_or_default();
    normalize_alpha(&mut pixels);
    Icon {
        source: source.to_string(),
        size,
        pixels,
    }
}

/// The shell hands back straight alpha for some icons and no alpha at all for
/// old 24-bit ones. D2D wants premultiplied BGRA.
pub fn normalize_alpha(px: &mut [u8]) {
    if px.is_empty() {
        return;
    }
    if px.chunks_exact(4).all(|p| p[3] == 0) {
        px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        return;
    }
    if px
        .chunks_exact(4)
        .any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3])
    {
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

pub struct Loader {
    tx: Sender<(String, u32)>,
}

impl Loader {
    /// Starts the worker; each finished icon is posted to `hwnd` as `msg` with a boxed `Icon` in lparam.
    pub fn start(hwnd: HWND, msg: u32) -> Loader {
        Self::pool(hwnd, msg, 1)
    }

    /// `threads` workers sharing one queue (Launchpad loads a page at once).
    pub fn pool(hwnd: HWND, msg: u32, threads: usize) -> Loader {
        let (tx, rx) = channel::<(String, u32)>();
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        let target = hwnd.0 as isize;
        for _ in 0..threads.max(1) {
            let rx = rx.clone();
            std::thread::spawn(move || {
            unsafe { _ = CoInitializeEx(None, COINIT_MULTITHREADED) };
            loop {
                let job = rx.lock().ok().and_then(|r| r.recv().ok());
                let Some((source, size)) = job else { break };
                let icon = Box::into_raw(Box::new(load(&source, size)));
                let posted = unsafe {
                    PostMessageW(
                        Some(HWND(target as *mut _)),
                        msg,
                        WPARAM(0),
                        LPARAM(icon as isize),
                    )
                };
                if posted.is_err() {
                    drop(unsafe { Box::from_raw(icon) }); // window is gone
                }
            }
            });
        }
        Loader { tx }
    }

    pub fn request(&self, source: &str, size: u32) {
        _ = self.tx.send((source.to_string(), size));
    }
}

/// Takes ownership of an icon posted by the worker.
/// Safety: `lp` must come from a `Loader` message and be taken exactly once.
pub unsafe fn take(lp: LPARAM) -> Box<Icon> {
    unsafe { Box::from_raw(lp.0 as *mut Icon) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_alpha_is_premultiplied() {
        let mut px = vec![200, 100, 50, 128];
        normalize_alpha(&mut px);
        assert_eq!(px, [100, 50, 25, 128]);
    }

    #[test]
    fn premultiplied_input_is_left_alone() {
        let mut px = vec![100, 50, 25, 128, 0, 0, 0, 0];
        normalize_alpha(&mut px);
        assert_eq!(px, [100, 50, 25, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn missing_alpha_becomes_opaque() {
        let mut px = vec![10, 20, 30, 0, 40, 50, 60, 0];
        normalize_alpha(&mut px);
        assert_eq!(px, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn loads_a_real_exe_icon() {
        unsafe { _ = CoInitializeEx(None, COINIT_MULTITHREADED) };
        let icon = load(r"C:\Windows\explorer.exe", 64);
        assert_eq!(icon.pixels.len(), 64 * 64 * 4);
        assert!(icon.pixels.chunks_exact(4).any(|p| p[3] == 255));
        assert!(
            icon.pixels.chunks_exact(4).any(|p| p[3] == 0),
            "corners should be transparent"
        );
    }
}
