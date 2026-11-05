//! Battery, volume and network state for the menu bar.
//!
//! Event driven: the OS calls back on its own threads, the callbacks only post
//! `WM_STATUS` to the menu bar, and the menu bar re-reads state on its thread.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::NetworkManagement::IpHelper::{GetNetworkConnectivityHint, NotifyNetworkConnectivityHintChange};
use windows::Win32::NetworkManagement::WiFi::*;
use windows::Win32::Networking::WinSock::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Power::{GetSystemPowerStatus, RegisterPowerSettingNotification, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_WINDOW_HANDLE, PostMessageW, WM_APP};
use windows::core::{PCWSTR, implement};

use crate::log;

pub const WM_STATUS: u32 = WM_APP + 3;
/// `WM_STATUS` wparam: state changed, re-read it.
pub const REFRESH: usize = 0;
/// `WM_STATUS` wparam: default playback device changed, rebind first.
pub const AUDIO_DEVICE: usize = 1;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Battery {
    pub percent: u8,
    pub plugged: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Volume {
    pub level: f32,
    pub muted: bool,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Net {
    #[default]
    Unknown,
    Offline,
    NoInternet,
    Wired,
    /// Signal quality 0..=100.
    Wifi(u8),
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Status {
    pub battery: Option<Battery>,
    pub volume: Option<Volume>,
    pub net: Net,
}

/// One menu bar icon; `under` is drawn faintly beneath it (empty Wi-Fi arcs).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Icon {
    pub glyph: char,
    pub under: Option<char>,
}

const WIFI_FULL: char = '\u{E701}';

pub fn battery_glyph(percent: u8, plugged: bool) -> char {
    let step = ((percent.min(100) as u32 + 5) / 10) as u8; // 0..=10, nearest tenth
    let code = match (plugged, step) {
        (false, 10) => 0xE83F,
        (false, s) => 0xE850 + s as u32,
        (true, 10) => 0xEA93,
        (true, 9) => 0xE83E,
        (true, s) => 0xE85A + s as u32,
    };
    char::from_u32(code).unwrap()
}

pub fn volume_glyph(level: f32, muted: bool) -> char {
    match level {
        _ if muted => '\u{E74F}',
        l if l <= 0.0 => '\u{E992}',
        l if l < 0.34 => '\u{E993}',
        l if l < 0.67 => '\u{E994}',
        _ => '\u{E995}',
    }
}

pub fn wifi_glyph(signal: u8) -> char {
    match signal {
        0..25 => '\u{E872}',
        25..50 => '\u{E873}',
        50..75 => '\u{E874}',
        _ => WIFI_FULL,
    }
}

/// Icons from right to left, macOS order: network, battery, volume.
/// Wired with internet shows nothing, as on macOS.
pub fn icons(st: &Status) -> Vec<Icon> {
    let mut out = Vec::new();
    match st.net {
        Net::Wifi(s) => out.push(Icon { glyph: wifi_glyph(s), under: Some(WIFI_FULL) }),
        Net::Offline => out.push(Icon { glyph: '\u{EB5E}', under: None }),
        Net::NoInternet => out.push(Icon { glyph: '\u{F384}', under: None }),
        Net::Wired | Net::Unknown => {}
    }
    if let Some(b) = st.battery {
        out.push(Icon { glyph: battery_glyph(b.percent, b.plugged), under: None });
    }
    if let Some(v) = st.volume {
        out.push(Icon { glyph: volume_glyph(v.level, v.muted), under: None });
    }
    out
}

fn post(hwnd: isize, what: usize) {
    unsafe { _ = PostMessageW(Some(HWND(hwnd as *mut c_void)), WM_STATUS, WPARAM(what), LPARAM(0)) };
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeChanged(isize);

impl IAudioEndpointVolumeCallback_Impl for VolumeChanged_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        post(self.0, REFRESH);
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DeviceChanged(isize);

impl IMMNotificationClient_Impl for DeviceChanged_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            post(self.0, AUDIO_DEVICE);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &windows::Win32::Foundation::PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

unsafe extern "system" fn on_connectivity(ctx: *const c_void, _: NL_NETWORK_CONNECTIVITY_HINT) {
    post(ctx as isize, REFRESH);
}

unsafe extern "system" fn on_wlan(data: *mut L2_NOTIFICATION_DATA, ctx: *mut c_void) {
    let Some(d) = (unsafe { data.as_ref() }) else { return };
    let code = d.NotificationCode as i32;
    let relevant = match d.NotificationSource {
        WLAN_NOTIFICATION_SOURCE_MSM => [wlan_notification_msm_signal_quality_change.0, wlan_notification_msm_connected.0, wlan_notification_msm_disconnected.0].contains(&code),
        WLAN_NOTIFICATION_SOURCE_ACM => [wlan_notification_acm_connection_complete.0, wlan_notification_acm_disconnected.0].contains(&code),
        _ => false,
    };
    if relevant {
        post(ctx as isize, REFRESH);
    }
}

pub struct Sources {
    enumerator: Option<IMMDeviceEnumerator>,
    volume_cb: IAudioEndpointVolumeCallback,
    _device_cb: IMMNotificationClient,
    endpoint: RefCell<Option<IAudioEndpointVolume>>,
    wlan: Option<HANDLE>,
}

impl Sources {
    /// Subscribes to every source. A source that fails is logged and its icon stays hidden.
    pub fn start(hwnd: HWND) -> Sources {
        let ctx = hwnd.0 as isize;
        let volume_cb: IAudioEndpointVolumeCallback = VolumeChanged(ctx).into();
        let device_cb: IMMNotificationClient = DeviceChanged(ctx).into();

        let enumerator = unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .inspect_err(|e| log::error(&format!("audio: {e}")))
            .ok();
        if let Some(en) = &enumerator {
            unsafe { _ = en.RegisterEndpointNotificationCallback(&device_cb) };
        }

        unsafe {
            for guid in [GUID_BATTERY_PERCENTAGE_REMAINING, GUID_ACDC_POWER_SOURCE] {
                if let Err(e) = RegisterPowerSettingNotification(HANDLE(hwnd.0), &guid, DEVICE_NOTIFY_WINDOW_HANDLE) {
                    log::error(&format!("power notification: {e}"));
                }
            }
            let mut hint_handle = HANDLE::default();
            let err = NotifyNetworkConnectivityHintChange(Some(on_connectivity), Some(ctx as *const c_void), false, &mut hint_handle);
            if err.is_err() {
                log::error(&format!("connectivity notification: {err:?}"));
            }
        }

        let wlan = unsafe {
            let mut version = 0;
            let mut handle = HANDLE::default();
            if WlanOpenHandle(2, None, &mut version, &mut handle) == 0 {
                WlanRegisterNotification(handle, WLAN_NOTIFICATION_SOURCE_ACM | WLAN_NOTIFICATION_SOURCE_MSM, true, Some(on_wlan), Some(ctx as *const c_void), None, None);
                Some(handle)
            } else {
                None // no Wi-Fi service, desktop on cable
            }
        };

        let sources = Sources { enumerator, volume_cb, _device_cb: device_cb, endpoint: RefCell::new(None), wlan };
        sources.bind_audio();
        sources
    }

    /// (Re)binds to the default playback device and its volume events.
    pub fn bind_audio(&self) {
        if let Some(old) = self.endpoint.borrow_mut().take() {
            unsafe { _ = old.UnregisterControlChangeNotify(&self.volume_cb) };
        }
        let Some(en) = &self.enumerator else { return };
        let endpoint = unsafe {
            en.GetDefaultAudioEndpoint(eRender, eConsole)
                .and_then(|d| d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None))
        };
        match endpoint {
            Ok(ep) => {
                unsafe { _ = ep.RegisterControlChangeNotify(&self.volume_cb) };
                *self.endpoint.borrow_mut() = Some(ep);
            }
            Err(e) => log::info(&format!("no playback device: {e}")),
        }
    }

    pub fn read(&self) -> Status {
        Status { battery: read_battery(), volume: self.read_volume(), net: self.read_net() }
    }

    fn read_volume(&self) -> Option<Volume> {
        let ep = self.endpoint.borrow();
        let ep = ep.as_ref()?;
        unsafe { Some(Volume { level: ep.GetMasterVolumeLevelScalar().ok()?, muted: ep.GetMute().ok()?.as_bool() }) }
    }

    #[allow(non_upper_case_globals)] // windows-rs constant names
    fn read_net(&self) -> Net {
        let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
        if unsafe { GetNetworkConnectivityHint(&mut hint) }.is_err() {
            return Net::Unknown;
        }
        let wifi = self.wlan.and_then(wifi_signal);
        match (hint.ConnectivityLevel, wifi) {
            (NetworkConnectivityLevelHintInternetAccess, Some(s)) => Net::Wifi(s),
            (NetworkConnectivityLevelHintInternetAccess, None) => Net::Wired,
            (NetworkConnectivityLevelHintLocalAccess | NetworkConnectivityLevelHintConstrainedInternetAccess, _) => Net::NoInternet,
            (NetworkConnectivityLevelHintNone, _) => Net::Offline,
            _ => Net::Unknown,
        }
    }
}

fn read_battery() -> Option<Battery> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps).ok()? };
    // 128: no system battery, 255: unknown.
    if ps.BatteryFlag & 128 != 0 || ps.BatteryFlag == 255 || ps.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery { percent: ps.BatteryLifePercent, plugged: ps.ACLineStatus == 1 })
}

/// Signal quality of the first connected Wi-Fi interface.
fn wifi_signal(wlan: HANDLE) -> Option<u8> {
    unsafe {
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(wlan, None, &mut list) != 0 || list.is_null() {
            return None;
        }
        let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), (*list).dwNumberOfItems as usize);
        let signal = items.iter().filter(|i| i.isState == wlan_interface_state_connected).find_map(|i| {
            let mut size = 0;
            let mut data: *mut c_void = std::ptr::null_mut();
            if WlanQueryInterface(wlan, &i.InterfaceGuid, wlan_intf_opcode_current_connection, None, &mut size, &mut data, None) != 0 || data.is_null() {
                return None;
            }
            let q = (*(data as *const WLAN_CONNECTION_ATTRIBUTES)).wlanAssociationAttributes.wlanSignalQuality;
            WlanFreeMemory(data);
            Some(q.min(100) as u8)
        });
        WlanFreeMemory(list as *const c_void);
        signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_steps() {
        assert_eq!(battery_glyph(100, false), '\u{E83F}');
        assert_eq!(battery_glyph(95, false), '\u{E83F}');
        assert_eq!(battery_glyph(94, false), '\u{E859}');
        assert_eq!(battery_glyph(4, false), '\u{E850}');
        assert_eq!(battery_glyph(0, false), '\u{E850}');
        assert_eq!(battery_glyph(100, true), '\u{EA93}');
        assert_eq!(battery_glyph(90, true), '\u{E83E}');
        assert_eq!(battery_glyph(80, true), '\u{E862}');
        assert_eq!(battery_glyph(0, true), '\u{E85A}');
        assert_eq!(battery_glyph(255, false), '\u{E83F}'); // clamped
    }

    #[test]
    fn volume_steps() {
        assert_eq!(volume_glyph(0.8, true), '\u{E74F}');
        assert_eq!(volume_glyph(0.0, false), '\u{E992}');
        assert_eq!(volume_glyph(0.2, false), '\u{E993}');
        assert_eq!(volume_glyph(0.5, false), '\u{E994}');
        assert_eq!(volume_glyph(1.0, false), '\u{E995}');
    }

    #[test]
    fn wifi_steps() {
        assert_eq!(wifi_glyph(0), '\u{E872}');
        assert_eq!(wifi_glyph(24), '\u{E872}');
        assert_eq!(wifi_glyph(25), '\u{E873}');
        assert_eq!(wifi_glyph(74), '\u{E874}');
        assert_eq!(wifi_glyph(100), WIFI_FULL);
    }

    #[test]
    fn icon_order_and_wired_hidden() {
        let st = Status {
            battery: Some(Battery { percent: 50, plugged: false }),
            volume: Some(Volume { level: 0.5, muted: false }),
            net: Net::Wifi(80),
        };
        let glyphs: Vec<char> = icons(&st).iter().map(|i| i.glyph).collect();
        assert_eq!(glyphs, [WIFI_FULL, battery_glyph(50, false), volume_glyph(0.5, false)]);
        let wired = Status { net: Net::Wired, ..st };
        assert_eq!(icons(&wired).len(), 2);
        assert!(icons(&Status::default()).is_empty());
    }
}
