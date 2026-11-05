//! Wi-Fi for the menu bar panel: radio on/off, nearby networks, connecting to
//! a known network. Windows 11 hides scan results from desktop apps without
//! location permission; that case is reported so the panel can say why.

use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::NetworkManagement::WiFi::*;
use windows::core::PCWSTR;

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// Signal quality 0..=100.
    pub quality: u8,
    pub secured: bool,
    /// Saved profile name: such networks connect with one click.
    pub profile: Option<String>,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Scan {
    Networks(Vec<Network>),
    NeedsLocation,
    NoAdapter,
}

/// Wi-Fi bars glyph (same as the menu bar) for a signal quality.
pub fn bars(quality: u8) -> u8 {
    match quality {
        0..20 => 1,
        20..45 => 2,
        45..70 => 3,
        _ => 4,
    }
}

/// One entry per SSID (the strongest), connected first, then by signal.
pub fn tidy(mut list: Vec<Network>) -> Vec<Network> {
    list.retain(|n| !n.ssid.is_empty());
    list.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.quality.cmp(&a.quality)));
    let mut out: Vec<Network> = Vec::new();
    for n in list {
        match out.iter_mut().find(|o| o.ssid == n.ssid) {
            Some(o) => {
                o.connected |= n.connected;
                if o.profile.is_none() {
                    o.profile = n.profile;
                }
            }
            None => out.push(n),
        }
    }
    out
}

struct Client(HANDLE);

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { WlanCloseHandle(self.0, None) };
    }
}

fn open() -> Option<(Client, windows::core::GUID)> {
    unsafe {
        let mut version = 0;
        let mut h = HANDLE::default();
        if WlanOpenHandle(2, None, &mut version, &mut h) != 0 {
            return None;
        }
        let client = Client(h);
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(h, None, &mut list) != 0 || list.is_null() {
            return None;
        }
        let guid = ((*list).dwNumberOfItems > 0).then(|| (*list).InterfaceInfo[0].InterfaceGuid);
        WlanFreeMemory(list as *const _);
        Some((client, guid?))
    }
}

fn utf16(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn scan() -> Scan {
    let Some((client, guid)) = open() else { return Scan::NoAdapter };
    unsafe {
        // Ask for fresh results next time; this call returns at once.
        WlanScan(client.0, &guid, None, None, None);
        let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
        let err = WlanGetAvailableNetworkList(client.0, &guid, 0, None, &mut list);
        if err == ERROR_ACCESS_DENIED.0 {
            return Scan::NeedsLocation;
        }
        if err != 0 || list.is_null() {
            return Scan::Networks(Vec::new());
        }
        let items = std::slice::from_raw_parts((*list).Network.as_ptr(), (*list).dwNumberOfItems as usize);
        let nets = items
            .iter()
            .map(|n| {
                let len = (n.dot11Ssid.uSSIDLength as usize).min(32);
                let profile = utf16(&n.strProfileName);
                Network {
                    ssid: String::from_utf8_lossy(&n.dot11Ssid.ucSSID[..len]).into_owned(),
                    quality: n.wlanSignalQuality.min(100) as u8,
                    secured: n.bSecurityEnabled.as_bool(),
                    profile: (n.dwFlags & WLAN_AVAILABLE_NETWORK_HAS_PROFILE != 0 && !profile.is_empty()).then_some(profile),
                    connected: n.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0,
                }
            })
            .collect();
        WlanFreeMemory(list as *const _);
        Scan::Networks(tidy(nets))
    }
}

/// Connects with the saved profile. Unknown networks need a password, which
/// the Windows network flyout asks for.
pub fn connect(profile: &str) -> bool {
    let Some((client, guid)) = open() else { return false };
    let name: Vec<u16> = profile.encode_utf16().chain([0]).collect();
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: PCWSTR(name.as_ptr()),
        dot11BssType: dot11_BSS_type_any,
        ..Default::default()
    };
    unsafe { WlanConnect(client.0, &guid, &params, None) == 0 }
}

/// Name of the current Wi-Fi connection, readable without location permission.
pub fn current_name() -> Option<String> {
    use windows::Networking::Connectivity::NetworkInformation;
    let p = NetworkInformation::GetInternetConnectionProfile().ok()?;
    if !p.IsWlanConnectionProfile().ok()? {
        return None;
    }
    p.ProfileName().ok().map(|n| n.to_string())
}

fn radio(kind: RadioKind) -> Option<Radio> {
    let radios = Radio::GetRadiosAsync().ok()?.join().ok()?;
    radios.into_iter().find(|r| r.Kind().is_ok_and(|k| k == kind))
}

fn radio_state(kind: RadioKind) -> Option<bool> {
    radio(kind)?.State().ok().map(|s| s == RadioState::On)
}

fn set_radio_state(kind: RadioKind, on: bool) -> bool {
    let Some(radio) = radio(kind) else { return false };
    let state = if on { RadioState::On } else { RadioState::Off };
    radio.SetStateAsync(state).ok().and_then(|op| op.join().ok()).is_some()
}

pub fn radio_on() -> Option<bool> {
    radio_state(RadioKind::WiFi)
}

pub fn set_radio(on: bool) -> bool {
    set_radio_state(RadioKind::WiFi, on)
}

/// Bluetooth radio (kept with the Wi-Fi one): `None` without an adapter.
pub fn bluetooth_on() -> Option<bool> {
    radio_state(RadioKind::Bluetooth)
}

pub fn set_bluetooth(on: bool) -> bool {
    set_radio_state(RadioKind::Bluetooth, on)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ssid: &str, quality: u8, connected: bool) -> Network {
        Network { ssid: ssid.into(), quality, secured: true, profile: None, connected }
    }

    #[test]
    fn tidy_dedupes_and_orders() {
        let out = tidy(vec![net("Ev", 40, false), net("Kafe", 90, false), net("Ev", 80, true), net("", 99, false)]);
        let names: Vec<&str> = out.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(names, ["Ev", "Kafe"], "connected first, hidden SSIDs dropped");
        assert!(out[0].connected);
        assert_eq!(out[0].quality, 80, "strongest copy kept");
    }

    #[test]
    fn bars_from_quality() {
        assert_eq!(bars(0), 1);
        assert_eq!(bars(44), 2);
        assert_eq!(bars(69), 3);
        assert_eq!(bars(100), 4);
    }
}
