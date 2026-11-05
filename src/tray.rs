//! Notification area icons in the menu bar, like macOS menu extras.
//!
//! `Shell_NotifyIcon` finds the taskbar with `FindWindow("Shell_TrayWnd")` and
//! sends it `WM_COPYDATA`. We keep a hidden window of that class above
//! Explorer's, record the icons, and pass every message on to Explorer, so its
//! own tray, notifications and the AppBar API (which uses the same window) keep
//! working. Apps re-add their icons when we broadcast "TaskbarCreated".

use std::cell::{Cell, OnceCell, RefCell};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{GUID, PCWSTR, w};

use crate::window::{self, Handler};

const CLASS: PCWSTR = w!("Shell_TrayWnd");
const SIGNATURE: u32 = 0x3475_3423;
const TRAY_DATA: usize = 1;
const TIMER_WATCH: usize = 1;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_STATE: u32 = 0x8;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x1;
const NIN_SELECT: u32 = WM_USER;

/// `NOTIFYICONDATAW` as the shell sends it: handles as 32-bit values, full size.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawNid {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawTrayData {
    signature: u32,
    message: u32,
    nid: RawNid,
}

/// One `Shell_NotifyIcon` call.
#[derive(Clone, Debug, PartialEq)]
pub struct Nid {
    pub hwnd: u32,
    pub uid: u32,
    pub flags: u32,
    pub callback: u32,
    pub hicon: u32,
    pub tip: String,
    pub state: u32,
    pub state_mask: u32,
    pub version: u32,
    pub guid: GUID,
}

/// Message and icon data from a tray `WM_COPYDATA` payload.
pub fn parse(data: &[u8]) -> Option<(u32, Nid)> {
    // Older senders may stop before the GUID; pad to the full layout.
    let mut buf = [0u8; size_of::<RawTrayData>()];
    let n = data.len().min(buf.len());
    if n < 8 + 24 {
        return None;
    }
    buf[..n].copy_from_slice(&data[..n]);
    let raw: RawTrayData = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const RawTrayData) };
    if raw.signature != SIGNATURE {
        return None;
    }
    let r = raw.nid;
    let end = r.tip.iter().position(|c| *c == 0).unwrap_or(r.tip.len());
    Some((
        raw.message,
        Nid {
            hwnd: r.hwnd,
            uid: r.uid,
            flags: r.flags,
            callback: r.callback,
            hicon: r.hicon,
            tip: String::from_utf16_lossy(&r.tip[..end]),
            state: r.state,
            state_mask: r.state_mask,
            version: r.version,
            guid: r.guid,
        },
    ))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub hwnd: u32,
    pub uid: u32,
    pub guid: Option<GUID>,
    pub callback: u32,
    pub version: u32,
    pub tip: String,
    pub hicon: u32,
    pub hidden: bool,
    /// Premultiplied BGRA, `ICON_DIP` at the menu bar's scale; empty until converted.
    pub pixels: Vec<u8>,
}

fn same(e: &Entry, n: &Nid) -> bool {
    match (n.flags & NIF_GUID != 0, e.guid) {
        (true, Some(g)) => g == n.guid,
        _ => e.hwnd == n.hwnd && e.uid == n.uid,
    }
}

/// Applies one call to the icon list; true when the menu bar must redraw.
/// A changed icon handle clears `pixels` (the caller converts it).
pub fn apply(list: &mut Vec<Entry>, message: u32, n: &Nid) -> bool {
    let found = list.iter().position(|e| same(e, n));
    match (message, found) {
        (NIM_DELETE, Some(i)) => {
            list.remove(i);
            true
        }
        (NIM_SETVERSION, Some(i)) => {
            list[i].version = n.version;
            false
        }
        (NIM_ADD, None) => {
            let mut e = Entry { hwnd: n.hwnd, uid: n.uid, ..Default::default() };
            if n.flags & NIF_GUID != 0 {
                e.guid = Some(n.guid);
            }
            update(&mut e, n);
            list.push(e);
            true
        }
        // ADD of a known icon (apps re-adding after "TaskbarCreated") updates it.
        (NIM_ADD | NIM_MODIFY, Some(i)) => {
            update(&mut list[i], n);
            true
        }
        _ => false,
    }
}

fn update(e: &mut Entry, n: &Nid) {
    if n.flags & NIF_MESSAGE != 0 {
        e.callback = n.callback;
    }
    if n.flags & NIF_ICON != 0 && n.hicon != e.hicon {
        e.hicon = n.hicon;
        e.pixels.clear();
    }
    if n.flags & NIF_TIP != 0 {
        e.tip = n.tip.clone();
    }
    if n.flags & NIF_STATE != 0 && n.state_mask & NIS_HIDDEN != 0 {
        e.hidden = n.state & NIS_HIDDEN != 0;
    }
}

/// Mouse messages for a click, in the form the icon's version expects:
/// (message, wparam, lparam) to post to the owner window.
pub fn click_messages(e: &Entry, right: bool, x: i32, y: i32) -> Vec<(u32, usize, isize)> {
    let (down, up, after) = if right {
        (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU)
    } else {
        (WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT)
    };
    let mut seq = vec![down, up];
    if e.version >= 3 {
        seq.push(after);
    }
    seq.into_iter()
        .map(|m| {
            if e.version >= 4 {
                // NOTIFYICON_VERSION_4: wparam = anchor point, lparam = message | id << 16.
                let wp = ((y as u16 as usize) << 16) | x as u16 as usize;
                let lp = ((e.uid as isize & 0xFFFF) << 16) | m as isize;
                (e.callback, wp, lp)
            } else {
                (e.callback, e.uid as usize, m as isize)
            }
        })
        .collect()
}

// ---- the window ----

pub const ICON_DIP: f32 = 16.0;

struct Tray {
    hwnd: Cell<HWND>,
    icons: RefCell<Vec<Entry>>,
    /// Main menu bar: told about changes with `crate::menubar::WM_TRAY`.
    bar: Cell<HWND>,
    icon_px: Cell<u32>,
    ticks: Cell<u32>,
}

thread_local! {
    static TRAY: OnceCell<&'static Tray> = const { OnceCell::new() };
}

/// Starts collecting tray icons for the menu bar `bar`.
pub fn start(bar: HWND) {
    let (_, dpi) = window::primary_monitor();
    let tray: &'static Tray = Box::leak(Box::new(Tray {
        hwnd: Cell::new(HWND::default()),
        icons: RefCell::new(Vec::new()),
        bar: Cell::new(bar),
        icon_px: Cell::new((ICON_DIP * crate::theme::scale(dpi)).round() as u32),
        ticks: Cell::new(0),
    }));
    let hwnd = match window::create_with(CLASS, WS_EX_TOOLWINDOW | WS_EX_TOPMOST, WS_POPUP, tray) {
        Ok(h) => h,
        Err(e) => return crate::log::error(&format!("tray window: {e}")),
    };
    tray.hwnd.set(hwnd);
    TRAY.with(|t| _ = t.set(tray));
    tray.stay_first();
    unsafe {
        SetTimer(Some(hwnd), TIMER_WATCH, 1000, None);
        // Every app with a tray icon adds it again, now through us.
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0));
    }
}

/// Visible icons, right to left order of the menu bar (newest nearest the clock).
pub fn icons() -> Vec<Entry> {
    TRAY.with(|t| t.get().map(|t| t.icons.borrow().iter().filter(|e| !e.hidden && !e.pixels.is_empty()).rev().cloned().collect()))
        .unwrap_or_default()
}

/// Forwards a click on icon `i` of `icons()` to its app. `x`, `y`: screen px under the icon.
pub fn click(i: usize, right: bool, x: i32, y: i32) {
    let Some(e) = icons().into_iter().nth(i) else { return };
    let owner = HWND(e.hwnd as usize as *mut _);
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(owner, Some(&mut pid));
        _ = AllowSetForegroundWindow(pid); // so its menu or window can come to the front
        for (msg, wp, lp) in click_messages(&e, right, x, y) {
            _ = PostMessageW(Some(owner), msg, WPARAM(wp), LPARAM(lp));
        }
    }
}

impl Tray {
    /// `Shell_NotifyIcon` talks to the first `Shell_TrayWnd` in z-order: ours.
    fn stay_first(&self) {
        let me = self.hwnd.get();
        unsafe {
            if FindWindowW(CLASS, None).ok() != Some(me) {
                _ = SetWindowPos(me, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn explorer(&self) -> Option<HWND> {
        let me = self.hwnd.get();
        let mut after = None;
        loop {
            let h = unsafe { FindWindowExW(None, after, CLASS, None) }.ok()?;
            if h != me {
                return Some(h);
            }
            after = Some(h);
        }
    }

    fn forward(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let Some(explorer) = self.explorer() else { return LRESULT(0) };
        let mut result = 0usize;
        unsafe { SendMessageTimeoutW(explorer, msg, wp, lp, SMTO_ABORTIFHUNG, 3000, Some(&mut result)) };
        LRESULT(result as isize)
    }

    fn on_copydata(&self, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
        if cds.dwData != TRAY_DATA || cds.lpData.is_null() {
            return self.forward(WM_COPYDATA, wp, lp); // AppBar and other shell calls
        }
        let data = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
        let forwarded = self.forward(WM_COPYDATA, wp, lp);
        if let Some((message, nid)) = parse(data) {
            let changed = apply(&mut self.icons.borrow_mut(), message, &nid);
            self.convert_missing();
            if changed {
                self.notify();
            }
            return LRESULT(1);
        }
        forwarded
    }

    /// Converts icons whose handle changed while the sender still waits (it may
    /// destroy the handle right after the call).
    fn convert_missing(&self) {
        let px = self.icon_px.get();
        for e in self.icons.borrow_mut().iter_mut().filter(|e| e.pixels.is_empty() && e.hicon != 0) {
            e.pixels = hicon_pixels(HICON(e.hicon as usize as *mut _), px).unwrap_or_default();
        }
    }

    fn notify(&self) {
        unsafe { _ = PostMessageW(Some(self.bar.get()), crate::menubar::WM_TRAY, WPARAM(0), LPARAM(0)) };
    }

    /// Drops icons whose app is gone without saying so (crashed, killed).
    fn prune(&self) {
        let before = self.icons.borrow().len();
        self.icons.borrow_mut().retain(|e| unsafe { IsWindow(Some(HWND(e.hwnd as usize as *mut _))) }.as_bool());
        if self.icons.borrow().len() != before {
            self.notify();
        }
    }
}

impl Handler for Tray {
    fn handle(&self, _: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_COPYDATA => Some(self.on_copydata(wp, lp)),
            WM_TIMER if wp.0 == TIMER_WATCH => {
                self.stay_first();
                self.prune();
                // Apps re-add their icons within a few seconds of "TaskbarCreated".
                if self.ticks.replace(self.ticks.get() + 1) == 5 {
                    let icons = self.icons.borrow();
                    let shown = icons.iter().filter(|e| !e.hidden && !e.pixels.is_empty()).count();
                    crate::log::info(&format!("tray: {} icons, {shown} shown", icons.len()));
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// Premultiplied BGRA pixels of an icon handle at `size` px (WIC handles the
/// mask and alpha of old and new icons alike).
pub fn hicon_pixels(hicon: HICON, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let bmp = wic.CreateBitmapFromHICON(hicon).ok()?;
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&bmp, size, size, WICBitmapInterpolationModeFant).ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let mut px = vec![0u8; (size * size * 4) as usize];
        conv.CopyPixels(std::ptr::null(), size * 4, &mut px).ok()?;
        Some(px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(message: u32, hwnd: u32, uid: u32, flags: u32, hicon: u32, tip: &str, version: u32) -> Vec<u8> {
        let mut nid: RawNid = unsafe { std::mem::zeroed() };
        nid.cb_size = size_of::<RawNid>() as u32;
        nid.hwnd = hwnd;
        nid.uid = uid;
        nid.flags = flags;
        nid.callback = WM_APP + 5;
        nid.hicon = hicon;
        for (d, s) in nid.tip.iter_mut().zip(tip.encode_utf16()) {
            *d = s;
        }
        nid.version = version;
        let raw = RawTrayData { signature: SIGNATURE, message, nid };
        let p = &raw as *const RawTrayData as *const u8;
        unsafe { std::slice::from_raw_parts(p, size_of::<RawTrayData>()) }.to_vec()
    }

    #[test]
    fn parses_shell_payload() {
        let (m, n) = parse(&payload(NIM_ADD, 0x1234, 7, NIF_ICON | NIF_TIP | NIF_MESSAGE, 0x99, "Ses", 0)).unwrap();
        assert_eq!((m, n.hwnd, n.uid, n.hicon, n.tip.as_str()), (NIM_ADD, 0x1234, 7, 0x99, "Ses"));
        let mut bad = payload(NIM_ADD, 1, 1, 0, 0, "", 0);
        bad[0] = 0;
        assert!(parse(&bad).is_none(), "wrong signature");
        assert!(parse(&[0u8; 10]).is_none(), "too short");
    }

    #[test]
    fn add_modify_hide_delete() {
        let mut list = Vec::new();
        let call = |l: &mut Vec<Entry>, m, flags, hicon, state| {
            let (_, mut n) = parse(&payload(m, 0x10, 1, flags, hicon, "a", 4)).unwrap();
            n.state = state;
            n.state_mask = NIS_HIDDEN;
            apply(l, m, &n)
        };
        assert!(call(&mut list, NIM_ADD, NIF_ICON | NIF_MESSAGE, 5, 0));
        assert_eq!(list.len(), 1);
        list[0].pixels = vec![1];
        assert!(call(&mut list, NIM_MODIFY, NIF_ICON, 6, 0));
        assert!(list[0].pixels.is_empty(), "new icon handle needs converting");
        assert!(call(&mut list, NIM_ADD, NIF_ICON, 6, 0), "re-add updates");
        assert_eq!(list.len(), 1);
        assert!(call(&mut list, NIM_MODIFY, NIF_STATE, 0, NIS_HIDDEN));
        assert!(list[0].hidden);
        assert!(!call(&mut list, NIM_SETVERSION, 0, 0, 0));
        assert_eq!(list[0].version, 4);
        assert!(call(&mut list, NIM_DELETE, 0, 0, 0));
        assert!(list.is_empty());
    }

    #[test]
    fn click_follows_icon_version() {
        let e = Entry { uid: 3, callback: 0x8001, version: 0, ..Default::default() };
        assert_eq!(click_messages(&e, false, 0, 0), [(0x8001, 3, WM_LBUTTONDOWN as isize), (0x8001, 3, WM_LBUTTONUP as isize)]);
        let v4 = Entry { version: 4, ..e };
        let msgs = click_messages(&v4, true, 100, 20);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2], (0x8001, (20 << 16) | 100, (3 << 16) | WM_CONTEXTMENU as isize));
    }
}
