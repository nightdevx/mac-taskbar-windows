//! Menu bar panels in the macOS style: calendar (clock), Wi-Fi, battery and
//! power mode, volume. One rounded blurred popup below the clicked item; rows
//! are rebuilt from live state on every open and after every action.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::gfx::{self, Gfx, Label};
use crate::popup::{self, Place, Popup};
use crate::theme::{self, Rgba};
use crate::window::{self, Handler};
use crate::{calendar, clock, power, status, wifi};

const CLASS: PCWSTR = w!("TaskbarPanel");
pub const WIDTH: f32 = 300.0;
const PAD: f32 = 8.0;
const SIDE: f32 = 14.0;
const CELL_H: f32 = 30.0;
const CAL_HEAD: f32 = 34.0;
const CAL_DAYS: f32 = 22.0;
const SWITCH_W: f32 = 34.0;
const SWITCH_H: f32 = 20.0;
const KNOB: f32 = 18.0;
const LOCK: char = '\u{E72E}';
const WIFI_FULL: char = '\u{E701}';

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Calendar,
    Wifi,
    Battery,
    Volume,
}

/// What clicking a row does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    None,
    Open(&'static str),
    WifiToggle,
    WifiConnect(usize),
    Power(power::Mode),
    Mute,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Header { title: String, switch: Option<bool> },
    Big(String),
    Note(String),
    Slider { icon: char, value: f32 },
    /// `badge`: small glyph right of the label (lock on secured networks).
    Choice { icon: Option<char>, label: String, badge: Option<char>, checked: bool, act: Act },
    Separator,
    Link { label: String, act: Act },
    Calendar,
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Header { .. } => 36.0,
            Row::Big(_) => 40.0,
            Row::Note(_) => 24.0,
            Row::Slider { .. } => 38.0,
            Row::Choice { .. } | Row::Link { .. } => 30.0,
            Row::Separator => 9.0,
            Row::Calendar => CAL_HEAD + CAL_DAYS + 6.0 * CELL_H + 4.0,
        }
    }
    fn link(label: &str, act: Act) -> Row {
        Row::Link { label: label.into(), act }
    }
}

pub fn layout(rows: &[Row]) -> (Vec<f32>, f32) {
    let mut y = PAD;
    let tops = rows.iter().map(|r| { let t = y; y += r.height(); t }).collect();
    (tops, y + PAD)
}

/// Part of a row under the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Part {
    Switch,
    Body,
    /// Slider track, value 0..=1.
    Slider(f32),
    SliderIcon,
    CalPrev,
    CalNext,
    CalTitle,
}

fn track(top: f32) -> D2D_RECT_F {
    let mid = top + 19.0;
    D2D_RECT_F { left: 44.0, top: mid - 2.0, right: WIDTH - SIDE - KNOB / 2.0, bottom: mid + 2.0 }
}

fn switch_rect(top: f32) -> D2D_RECT_F {
    let mid = top + 18.0;
    D2D_RECT_F { left: WIDTH - SIDE - SWITCH_W, top: mid - SWITCH_H / 2.0, right: WIDTH - SIDE, bottom: mid + SWITCH_H / 2.0 }
}

pub fn hit(rows: &[Row], x: f32, y: f32) -> Option<(usize, Part)> {
    let (tops, _) = layout(rows);
    let i = rows.iter().zip(&tops).position(|(r, t)| y >= *t && y < t + r.height())?;
    let top = tops[i];
    let part = match &rows[i] {
        Row::Header { switch: Some(_), .. } if x >= WIDTH - SIDE - SWITCH_W - 6.0 => Part::Switch,
        Row::Slider { .. } if x < 40.0 => Part::SliderIcon,
        Row::Slider { .. } => {
            let t = track(top);
            Part::Slider(((x - t.left) / (t.right - t.left)).clamp(0.0, 1.0))
        }
        Row::Calendar if y < top + CAL_HEAD && x >= WIDTH - SIDE - 24.0 => Part::CalNext,
        Row::Calendar if y < top + CAL_HEAD && x >= WIDTH - SIDE - 52.0 => Part::CalPrev,
        Row::Calendar if y < top + CAL_HEAD => Part::CalTitle,
        Row::Choice { .. } | Row::Link { .. } | Row::Calendar => Part::Body,
        _ => return None,
    };
    Some((i, part))
}

// ---- rows per panel ----

pub fn calendar_rows() -> Vec<Row> {
    vec![Row::Calendar, Row::Separator, Row::link("Tarih ve saat ayarları…", Act::Open("ms-settings:dateandtime"))]
}

pub fn wifi_rows(on: Option<bool>, scan: &wifi::Scan, current: Option<&str>) -> Vec<Row> {
    let mut rows = vec![Row::Header { title: "Wi-Fi".into(), switch: on }];
    if on != Some(false) {
        let choice = |i: usize, n: &wifi::Network| Row::Choice {
            icon: Some(status::wifi_glyph(wifi::bars(n.quality))),
            label: n.ssid.clone(),
            badge: n.secured.then_some(LOCK),
            checked: n.connected,
            act: Act::WifiConnect(i),
        };
        match scan {
            wifi::Scan::Networks(list) if !list.is_empty() => rows.extend(list.iter().enumerate().take(8).map(|(i, n)| choice(i, n))),
            wifi::Scan::NoAdapter => rows.push(Row::Note("Wi-Fi bağdaştırıcısı bulunamadı".into())),
            other => {
                if let Some(name) = current {
                    rows.push(Row::Choice { icon: Some(status::wifi_glyph(4)), label: name.into(), badge: None, checked: true, act: Act::None });
                }
                if *other == wifi::Scan::NeedsLocation {
                    rows.push(Row::Note("Ağ listesi için konum izni gerekiyor".into()));
                    rows.push(Row::link("Konum iznini aç…", Act::Open("ms-settings:privacy-location")));
                }
            }
        }
    }
    rows.push(Row::Separator);
    rows.push(Row::link("Diğer ağlar…", Act::Open("ms-availablenetworks:")));
    rows.push(Row::link("Wi-Fi ayarları…", Act::Open("ms-settings:network-wifi")));
    rows
}

pub fn battery_rows(battery: Option<status::Battery>, remaining: Option<u32>, mode: Option<power::Mode>) -> Vec<Row> {
    let mut rows = vec![Row::Header { title: "Pil".into(), switch: None }];
    if let Some(b) = battery {
        rows.push(Row::Big(clock::percent(b.percent)));
        let state = match (b.plugged, b.percent) {
            (true, 100) => "Prize takılı, dolu".to_string(),
            (true, _) => "Şarj oluyor".to_string(),
            (false, _) => remaining.map_or("Pil gücü".into(), power::format_remaining),
        };
        rows.push(Row::Note(state));
    }
    rows.push(Row::Separator);
    rows.push(Row::Note("Güç modu".into()));
    for m in power::MODES {
        rows.push(Row::Choice { icon: None, label: m.label().into(), badge: None, checked: mode == Some(m), act: Act::Power(m) });
    }
    rows.push(Row::Separator);
    rows.push(Row::link("Pil ve güç ayarları…", Act::Open("ms-settings:powersleep")));
    rows
}

pub fn volume_rows(volume: Option<status::Volume>) -> Vec<Row> {
    let mut rows = vec![Row::Header { title: "Ses".into(), switch: None }];
    match volume {
        Some(v) => rows.push(Row::Slider { icon: status::volume_glyph(v.level, v.muted), value: v.level }),
        None => rows.push(Row::Note("Çıkış aygıtı yok".into())),
    }
    rows.push(Row::Separator);
    rows.push(Row::link("Ses ayarları…", Act::Open("ms-settings:sound")));
    rows
}

// ---- drawing ----

pub struct Fonts {
    text: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    small: IDWriteTextFormat,
    big: IDWriteTextFormat,
    icons: IDWriteTextFormat,
    small_icons: IDWriteTextFormat,
    cap: f32,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory) -> Result<Self> {
        let f = |size, weight| gfx::text_format(dwrite, theme::TEXT_FONT, size, weight);
        Ok(Fonts {
            text: f(13.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            bold: f(13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
            small: f(11.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            big: f(24.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
            icons: gfx::text_format(dwrite, theme::ICON_FONT, 15.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            small_icons: gfx::text_format(dwrite, theme::ICON_FONT, 11.0, DWRITE_FONT_WEIGHT_REGULAR)?,
            cap: gfx::cap_height(dwrite, theme::TEXT_FONT, 13.0)?,
        })
    }
}

pub struct View<'a> {
    pub rows: &'a [Row],
    pub hover: Option<usize>,
    pub month: (i32, u32),
    pub today: (i32, u32, u32),
    pub palette: theme::Palette,
}

fn rr(r: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }
}

fn circle(x: f32, y: f32, r: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r }
}

pub fn draw(dc: &ID2D1DeviceContext, dwrite: &IDWriteFactory, f: &Fonts, v: &View) -> Result<()> {
    let p = v.palette;
    let (tops, _) = layout(v.rows);
    let small_cap = f.cap * 11.0 / 13.0;
    unsafe {
        let brush = |c: Rgba| dc.CreateSolidColorBrush(&c.d2d(), None);
        let text = brush(p.text)?;
        let secondary = brush(Rgba(p.text.0, p.text.1, p.text.2, p.text.3 / 2 + 10))?;
        let accent_c = theme::accent();
        let accent = brush(accent_c)?;
        let hover = brush(p.hover)?;
        let line = brush(Rgba(p.text.0, p.text.1, p.text.2, 34))?;
        let faint = brush(p.faint)?;
        let white = brush(Rgba(255, 255, 255, 255))?;
        let track_c = Rgba(p.text.0, p.text.1, p.text.2, 46);
        for (i, (row, top)) in v.rows.iter().zip(&tops).enumerate() {
            let top = *top;
            let band = top * 2.0 + row.height(); // centres text in this row
            let hot = v.hover == Some(i);
            let body = D2D_RECT_F { left: 5.0, top, right: WIDTH - 5.0, bottom: top + row.height() };
            match row {
                Row::Header { title, switch } => {
                    Label::new(dwrite, title, &f.bold)?.draw_text(dc, SIDE, band, f.cap, &text);
                    if let Some(on) = switch {
                        let s = switch_rect(top);
                        dc.FillRoundedRectangle(&rr(s, SWITCH_H / 2.0), &brush(if *on { accent_c } else { track_c })?);
                        let r = SWITCH_H / 2.0 - 2.0;
                        let kx = if *on { s.right - 2.0 - r } else { s.left + 2.0 + r };
                        dc.FillEllipse(&circle(kx, (s.top + s.bottom) / 2.0, r), &white);
                    }
                }
                Row::Big(s) => Label::new(dwrite, s, &f.big)?.draw(dc, SIDE, band, &text),
                Row::Note(s) => Label::new(dwrite, s, &f.small)?.draw_text(dc, SIDE, band, small_cap, &secondary),
                Row::Separator => {
                    let y = top + 4.5;
                    dc.FillRectangle(&D2D_RECT_F { left: SIDE, top: y - 0.5, right: WIDTH - SIDE, bottom: y + 0.5 }, &line);
                }
                Row::Slider { icon, value } => {
                    Label::new(dwrite, &icon.to_string(), &f.icons)?.draw(dc, SIDE, band, &text);
                    let t = track(top);
                    let kx = t.left + value.clamp(0.0, 1.0) * (t.right - t.left);
                    dc.FillRoundedRectangle(&rr(t, 2.0), &brush(track_c)?);
                    dc.FillRoundedRectangle(&rr(D2D_RECT_F { right: kx, ..t }, 2.0), &accent);
                    let ky = (t.top + t.bottom) / 2.0;
                    dc.FillEllipse(&circle(kx, ky + 0.75, KNOB / 2.0 + 0.5), &brush(Rgba(0, 0, 0, 70))?);
                    dc.FillEllipse(&circle(kx, ky, KNOB / 2.0), &white);
                }
                Row::Choice { icon, label, badge, checked, .. } => {
                    if hot {
                        dc.FillRoundedRectangle(&rr(body, 6.0), &hover);
                    }
                    let mut x = SIDE;
                    if let Some(g) = icon {
                        // Wi-Fi bars over faint empty arcs, as in the menu bar.
                        if ['\u{E872}', '\u{E873}', '\u{E874}'].contains(g) {
                            Label::new(dwrite, &WIFI_FULL.to_string(), &f.icons)?.draw(dc, x, band, &faint);
                        }
                        Label::new(dwrite, &g.to_string(), &f.icons)?.draw(dc, x, band, &text);
                        x += 26.0;
                    }
                    let mut max = WIDTH - x - SIDE - 22.0;
                    if let Some(b) = badge {
                        let mark = Label::new(dwrite, &b.to_string(), &f.small_icons)?;
                        mark.draw(dc, WIDTH - SIDE - 22.0 - mark.width, band, &secondary);
                        max -= mark.width + 6.0;
                    }
                    Label::fitted(dwrite, label, &f.text, max)?.draw_text(dc, x, band, f.cap, &text);
                    if *checked {
                        let mark = Label::new(dwrite, "\u{E73E}", &f.icons)?;
                        mark.draw(dc, WIDTH - SIDE - mark.width, band, &accent);
                    }
                }
                Row::Link { label, .. } => {
                    if hot {
                        dc.FillRoundedRectangle(&rr(body, 6.0), &hover);
                    }
                    Label::new(dwrite, label, &f.text)?.draw_text(dc, SIDE, band, f.cap, &text);
                }
                Row::Calendar => draw_calendar(dc, dwrite, f, v, top, &text, &secondary, &accent, &white)?,
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_calendar(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    f: &Fonts,
    v: &View,
    top: f32,
    text: &ID2D1SolidColorBrush,
    secondary: &ID2D1SolidColorBrush,
    accent: &ID2D1SolidColorBrush,
    white: &ID2D1SolidColorBrush,
) -> Result<()> {
    let (y, m) = v.month;
    let head_band = top * 2.0 + CAL_HEAD;
    Label::new(dwrite, &calendar::title(y, m), &f.bold)?.draw_text(dc, SIDE, head_band, f.cap, text);
    for (glyph, x) in [("\u{E76B}", WIDTH - SIDE - 48.0), ("\u{E76C}", WIDTH - SIDE - 20.0)] {
        Label::new(dwrite, glyph, &f.small_icons)?.draw(dc, x, head_band, text);
    }
    let first = calendar::first_weekday();
    let cell_w = (WIDTH - 2.0 * SIDE) / 7.0;
    let names_top = top + CAL_HEAD;
    for (i, name) in calendar::weekday_names(first).iter().enumerate() {
        let l = Label::new(dwrite, name, &f.small)?;
        l.draw_text(dc, SIDE + i as f32 * cell_w + (cell_w - l.width) / 2.0, names_top * 2.0 + CAL_DAYS, f.cap * 11.0 / 13.0, secondary);
    }
    let grid_top = names_top + CAL_DAYS;
    for (r, week) in calendar::grid(y, m, first).iter().enumerate() {
        for (c, day) in week.iter().enumerate() {
            let Some(d) = day else { continue };
            let cx = SIDE + c as f32 * cell_w + cell_w / 2.0;
            let cy = grid_top + r as f32 * CELL_H + CELL_H / 2.0;
            let today = (y, m, *d as u32) == v.today;
            if today {
                unsafe { dc.FillEllipse(&circle(cx, cy, 13.0), accent) };
            }
            let l = Label::new(dwrite, &d.to_string(), &f.text)?;
            l.draw_text(dc, cx - l.width / 2.0, cy * 2.0, f.cap, if today { white } else { text });
        }
    }
    Ok(())
}

// ---- window ----

struct Panel {
    popup: OnceCell<Popup>,
    gfx: Rc<Gfx>,
    fonts: Fonts,
    kind: Cell<Kind>,
    rows: RefCell<Vec<Row>>,
    month: Cell<(i32, u32)>,
    networks: RefCell<Vec<wifi::Network>>,
    volume: RefCell<Option<IAudioEndpointVolume>>,
    hover: Cell<Option<usize>>,
    dragging: Cell<bool>,
    anchor: Cell<(i32, i32)>,
    closed: Cell<Option<(Kind, Instant)>>,
}

thread_local! {
    static PANEL: OnceCell<&'static Panel> = const { OnceCell::new() };
}

fn get() -> Option<&'static Panel> {
    PANEL.with(|p| p.get().copied())
}

pub fn init(gfx: Rc<Gfx>) -> Result<()> {
    let p: &'static Panel = Box::leak(Box::new(Panel {
        fonts: Fonts::new(&gfx.dwrite)?,
        gfx: gfx.clone(),
        popup: OnceCell::new(),
        kind: Cell::new(Kind::Calendar),
        rows: RefCell::new(Vec::new()),
        month: Cell::new((2000, 1)),
        networks: RefCell::new(Vec::new()),
        volume: RefCell::new(None),
        hover: Cell::new(None),
        dragging: Cell::new(false),
        anchor: Cell::new((0, 0)),
        closed: Cell::new(None),
    }));
    _ = p.popup.set(Popup::create(gfx, CLASS, true, p)?);
    PANEL.with(|c| _ = c.set(p));
    Ok(())
}

/// Opens `kind` below the menu bar item centred at `x` (screen px), its top at `y`.
/// A second click on the same item closes it, like macOS.
pub fn toggle(kind: Kind, x: i32, y: i32) {
    let Some(p) = get() else { return };
    // The click that closed it (focus moved to the bar) must not reopen it.
    if let Some((k, t)) = p.closed.get()
        && k == kind
        && t.elapsed().as_millis() < 300
    {
        return;
    }
    p.kind.set(kind);
    p.anchor.set((x, y));
    p.hover.set(None);
    let now = clock::now();
    p.month.set((now.wYear as i32, now.wMonth as u32));
    if kind == Kind::Volume {
        *p.volume.borrow_mut() = endpoint();
    }
    p.rebuild();
}

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

impl Panel {
    fn popup(&self) -> &Popup {
        self.popup.get().expect("panel popup")
    }

    fn read_volume(&self) -> Option<status::Volume> {
        let ep = self.volume.borrow();
        let ep = ep.as_ref()?;
        unsafe { Some(status::Volume { level: ep.GetMasterVolumeLevelScalar().ok()?, muted: ep.GetMute().ok()?.as_bool() }) }
    }

    /// Reads live state, rebuilds the rows and shows the panel at its size.
    fn rebuild(&self) {
        let rows = match self.kind.get() {
            Kind::Calendar => calendar_rows(),
            Kind::Battery => battery_rows(status::read_battery(), power::remaining(), power::current()),
            Kind::Volume => volume_rows(self.read_volume()),
            Kind::Wifi => {
                let on = wifi::radio_on();
                let scan = if on == Some(false) { wifi::Scan::Networks(Vec::new()) } else { wifi::scan() };
                *self.networks.borrow_mut() = match &scan {
                    wifi::Scan::Networks(n) => n.clone(),
                    _ => Vec::new(),
                };
                let current = wifi::current_name();
                wifi_rows(on, &scan, current.as_deref())
            }
        };
        let (_, h) = layout(&rows);
        *self.rows.borrow_mut() = rows;
        let (monitor, dpi) = window::primary_monitor();
        let (x, y) = self.anchor.get();
        self.popup().show(x, Place::Below(y), monitor, dpi, WIDTH, h, |dc| self.draw(dc));
    }

    fn draw(&self, dc: &ID2D1DeviceContext) -> Result<()> {
        let now = clock::now();
        let view = View {
            rows: &self.rows.borrow(),
            hover: self.hover.get(),
            month: self.month.get(),
            today: (now.wYear as i32, now.wMonth as u32, now.wDay as u32),
            palette: theme::system_palette(),
        };
        draw(dc, &self.gfx.dwrite, &self.fonts, &view)
    }

    fn redraw(&self) {
        self.popup().redraw(|dc| self.draw(dc));
    }

    fn close(&self) {
        self.popup().hide();
        self.closed.set(Some((self.kind.get(), Instant::now())));
        *self.volume.borrow_mut() = None;
    }

    fn set_volume(&self, t: f32) {
        if let Some(ep) = self.volume.borrow().as_ref() {
            unsafe { _ = ep.SetMasterVolumeLevelScalar(t, std::ptr::null()) };
        }
    }

    fn act(&self, act: Act) {
        match act {
            Act::None => {}
            Act::Open(uri) => {
                self.close();
                unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(uri), None, None, SW_SHOWNORMAL) };
                return;
            }
            Act::WifiToggle => {
                let on = wifi::radio_on().unwrap_or(true);
                wifi::set_radio(!on);
            }
            Act::WifiConnect(i) => {
                let net = self.networks.borrow().get(i).cloned();
                match net.and_then(|n| n.profile) {
                    Some(profile) => {
                        wifi::connect(&profile);
                    }
                    // Unknown network: the Windows flyout asks for the password.
                    None => return self.act(Act::Open("ms-availablenetworks:")),
                }
            }
            Act::Power(mode) => {
                power::set(mode);
            }
            Act::Mute => {
                if let Some(ep) = self.volume.borrow().as_ref() {
                    unsafe {
                        let muted = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                        _ = ep.SetMute(!muted, std::ptr::null());
                    }
                }
            }
        }
        self.rebuild();
    }

    fn press(&self, x: f32, y: f32) {
        let rows = self.rows.borrow().clone();
        let Some((i, part)) = hit(&rows, x, y) else { return };
        match (&rows[i], part) {
            (Row::Header { switch: Some(_), .. }, Part::Switch) => self.act(Act::WifiToggle),
            (Row::Slider { .. }, Part::SliderIcon) => self.act(Act::Mute),
            (Row::Slider { .. }, Part::Slider(t)) => {
                self.dragging.set(true);
                unsafe { SetCapture(self.popup().hwnd) };
                self.set_volume(t);
                self.rebuild();
            }
            (Row::Calendar, Part::CalPrev | Part::CalNext) => {
                let (yy, mm) = self.month.get();
                self.month.set(calendar::shift(yy, mm, if part == Part::CalNext { 1 } else { -1 }));
                self.redraw();
            }
            (Row::Calendar, Part::CalTitle) => {
                let now = clock::now();
                self.month.set((now.wYear as i32, now.wMonth as u32));
                self.redraw();
            }
            (Row::Choice { act, .. } | Row::Link { act, .. }, _) => self.act(*act),
            _ => {}
        }
    }
}

impl Handler for Panel {
    fn handle(&self, _: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = popup::point(lp, self.popup().scale());
                if self.dragging.get() {
                    let (tops, _) = layout(&self.rows.borrow());
                    if let Some((i, _)) = self.rows.borrow().iter().enumerate().find(|(_, r)| matches!(r, Row::Slider { .. })) {
                        let t = track(tops[i]);
                        self.set_volume(((x - t.left) / (t.right - t.left)).clamp(0.0, 1.0));
                        self.rebuild();
                    }
                    return Some(LRESULT(0));
                }
                let h = hit(&self.rows.borrow(), x, y).filter(|(_, p)| *p == Part::Body).map(|(i, _)| i);
                if self.hover.replace(h) != h {
                    self.redraw();
                }
            }
            WM_LBUTTONDOWN => {
                let (x, y) = popup::point(lp, self.popup().scale());
                self.press(x, y);
            }
            WM_LBUTTONUP => {
                if self.dragging.replace(false) {
                    unsafe { _ = ReleaseCapture() };
                }
            }
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => self.close(),
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                if !self.dragging.get() {
                    self.close();
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_switch_slider_and_calendar_arrows() {
        let rows = vec![Row::Header { title: "Wi-Fi".into(), switch: Some(true) }, Row::Slider { icon: 'x', value: 0.5 }, Row::Calendar];
        let (tops, _) = layout(&rows);
        assert_eq!(hit(&rows, WIDTH - SIDE - 10.0, tops[0] + 10.0), Some((0, Part::Switch)));
        assert_eq!(hit(&rows, 20.0, tops[0] + 10.0), None, "header title is not a control");
        assert_eq!(hit(&rows, 20.0, tops[1] + 10.0), Some((1, Part::SliderIcon)));
        let t = track(tops[1]);
        assert_eq!(hit(&rows, t.left, tops[1] + 10.0), Some((1, Part::Slider(0.0))));
        assert_eq!(hit(&rows, t.right + 50.0, tops[1] + 10.0), Some((1, Part::Slider(1.0))));
        assert_eq!(hit(&rows, WIDTH - SIDE - 10.0, tops[2] + 5.0), Some((2, Part::CalNext)));
        assert_eq!(hit(&rows, WIDTH - SIDE - 40.0, tops[2] + 5.0), Some((2, Part::CalPrev)));
        assert_eq!(hit(&rows, 30.0, tops[2] + 5.0), Some((2, Part::CalTitle)));
    }

    #[test]
    fn wifi_rows_cover_off_location_and_networks() {
        let off = wifi_rows(Some(false), &wifi::Scan::Networks(vec![]), None);
        assert!(!off.iter().any(|r| matches!(r, Row::Choice { .. })), "no list while Wi-Fi is off");
        let denied = wifi_rows(Some(true), &wifi::Scan::NeedsLocation, Some("Ev"));
        assert!(denied.iter().any(|r| matches!(r, Row::Choice { checked: true, .. })), "current network still shown");
        assert!(denied.iter().any(|r| matches!(r, Row::Link { act: Act::Open("ms-settings:privacy-location"), .. })));
        let net = wifi::Network { ssid: "Kafe".into(), quality: 80, secured: true, profile: None, connected: false };
        let list = wifi_rows(Some(true), &wifi::Scan::Networks(vec![net]), None);
        assert!(list.iter().any(|r| matches!(r, Row::Choice { act: Act::WifiConnect(0), .. })));
    }

    #[test]
    fn battery_rows_mark_the_current_mode() {
        let rows = battery_rows(Some(status::Battery { percent: 64, plugged: false }), Some(3 * 3600), Some(power::Mode::Balanced));
        let checked: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Choice { label, checked: true, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(checked, ["Dengeli"]);
        assert!(rows.contains(&Row::Note("3 sa 0 dk kaldı".into())));
    }
}
