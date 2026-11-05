//! Who owns a window or a pinned shortcut: exe path, AppUserModelID and a
//! human display name. Dock entries group windows by this identity.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HWND, PROPERTYKEY};
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_Link_TargetParsingPath};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    GPS_DEFAULT, IPropertyStore, SHGetPropertyStoreForWindow, SHGetPropertyStoreFromParsingName,
};
use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{HSTRING, PWSTR};

fn take_pwstr(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string().ok() };
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s.filter(|s| !s.is_empty())
}

/// Exe path of the window's process and, for packaged (Store) apps, the
/// package AUMID, which their windows do not expose as a property.
pub fn process_identity(hwnd: HWND) -> Option<(String, Option<String>)> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let exe = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()
    .map(|_| String::from_utf16_lossy(&buf[..len as usize]));
    let mut len = buf.len() as u32;
    let package =
        (unsafe { GetApplicationUserModelId(process, &mut len, Some(PWSTR(buf.as_mut_ptr()))) }
            == ERROR_SUCCESS)
            .then(|| String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]));
    unsafe { _ = CloseHandle(process) };
    Some((exe?, package))
}

/// What a dock entry stands for. Either side may be unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppIdentity {
    pub aumid: Option<String>,
    /// Lower-case full exe path; `None` for Store app frames (ApplicationFrameHost hosts them all).
    pub exe: Option<String>,
}

impl AppIdentity {
    /// Same app when the AUMIDs match or the exes match. Exe matching groups all
    /// windows of an app (Brave profiles have their own AUMIDs), as the macOS dock does.
    pub fn same_app(&self, other: &AppIdentity) -> bool {
        let eq = |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
        eq(&self.aumid, &other.aumid) || eq(&self.exe, &other.exe)
    }
}

fn store_string(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let s = PropVariantToStringAlloc(&value).ok().and_then(take_pwstr);
        _ = PropVariantClear(&mut value);
        s
    }
}

/// AUMID set explicitly on the window (Store app frames, browsers, Terminal).
pub fn window_aumid(hwnd: HWND) -> Option<String> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd).ok()? };
    store_string(&store, &PKEY_AppUserModel_ID)
}

pub fn window_identity(hwnd: HWND) -> Option<AppIdentity> {
    let (exe, package) = process_identity(hwnd)?;
    let name = file_name_lower(&exe);
    // Explorer windows carry no AUMID, but the pinned "File Explorer" shortcut does.
    let implicit = (name == "explorer.exe").then(|| "Microsoft.Windows.Explorer".to_string());
    let aumid = package.or_else(|| window_aumid(hwnd)).or(implicit);
    let exe = (name != "applicationframehost.exe").then(|| exe.to_lowercase());
    Some(AppIdentity { aumid, exe })
}

/// Human name for a dock source: Start name of an AUMID, the shell's (localised)
/// name of a shortcut, the description of an exe.
pub fn source_name(source: &str) -> String {
    if let Some(aumid) = source.strip_prefix(r"shell:AppsFolder\") {
        return aumid_name(aumid).unwrap_or_else(|| aumid.to_string());
    }
    if source.to_ascii_lowercase().ends_with(".lnk")
        && let Some(name) = unsafe {
            SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(source), None)
                .and_then(|i| i.GetDisplayName(SIGDN_NORMALDISPLAY))
                .ok()
                .and_then(take_pwstr)
        }
    {
        return name;
    }
    display_name_fallback(file_description(source).as_deref(), source)
}

/// Identity of a pin source: a .lnk, `shell:AppsFolder\<AUMID>` or an exe path.
pub fn source_identity(source: &str) -> Option<AppIdentity> {
    if source.to_ascii_lowercase().ends_with(".lnk") {
        return link_identity(source);
    }
    if let Some(aumid) = source.strip_prefix(r"shell:AppsFolder\") {
        return Some(AppIdentity {
            aumid: Some(aumid.to_string()),
            exe: None,
        });
    }
    Some(AppIdentity {
        aumid: None,
        exe: Some(source.to_lowercase()),
    })
}

/// Identity of a pinned shortcut (.lnk): its AUMID property and target exe.
pub fn link_identity(path: &str) -> Option<AppIdentity> {
    let store: IPropertyStore =
        unsafe { SHGetPropertyStoreFromParsingName(&HSTRING::from(path), None, GPS_DEFAULT).ok()? };
    let exe = store_string(&store, &PKEY_Link_TargetParsingPath)
        .filter(|t| t.to_ascii_lowercase().ends_with(".exe"))
        .map(|t| t.to_lowercase());
    let aumid = store_string(&store, &PKEY_AppUserModel_ID);
    (aumid.is_some() || exe.is_some()).then_some(AppIdentity { aumid, exe })
}

thread_local! {
    static AUMID_NAMES: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
}

/// Start menu name for an AUMID ("WhatsApp", "Hesap Makinesi"), cached per AUMID.
/// Measured: the first lookup loads the shell Apps folder (139 ms, +178 handles),
/// later lookups cost 43 ms each, so they are cached.
// ponytail: first lookup still runs on the UI thread; move to the icon worker (F2.3) if it shows.
pub fn aumid_name(aumid: &str) -> Option<String> {
    if aumid.is_empty() {
        return None; // would name the Apps folder itself
    }
    if let Some(hit) = AUMID_NAMES.with_borrow(|m| m.get(aumid).cloned()) {
        return hit;
    }
    let name = unsafe {
        let path = HSTRING::from(format!(r"shell:AppsFolder\{aumid}"));
        SHCreateItemFromParsingName::<_, _, IShellItem>(&path, None)
            .and_then(|item| item.GetDisplayName(SIGDN_NORMALDISPLAY))
            .ok()
            .and_then(take_pwstr)
    };
    AUMID_NAMES.with_borrow_mut(|m| m.insert(aumid.to_string(), name.clone()));
    name
}

/// `FileDescription` from the exe's version resource.
pub fn file_description(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, data.as_mut_ptr() as *mut c_void).ok()?;

        let query = |key: &str| -> Option<(*const c_void, u32)> {
            let mut p = std::ptr::null_mut();
            let mut len = 0;
            VerQueryValueW(
                data.as_ptr() as *const c_void,
                &HSTRING::from(key),
                &mut p,
                &mut len,
            )
            .as_bool()
            .then_some((p as *const c_void, len))
        };
        // Language/codepage pairs the file declares, then the usual English ones.
        let mut langs: Vec<(u16, u16)> = query(r"\VarFileInfo\Translation")
            .map(|(p, len)| {
                std::slice::from_raw_parts(p as *const (u16, u16), len as usize / 4).to_vec()
            })
            .unwrap_or_default();
        langs.extend([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
        langs.into_iter().find_map(|(lang, cp)| {
            let (p, len) = query(&format!(
                r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription"
            ))?;
            let s =
                String::from_utf16_lossy(std::slice::from_raw_parts(p as *const u16, len as usize));
            let s = s.trim_end_matches('\0').trim().to_string();
            (!s.is_empty()).then_some(s)
        })
    }
}

/// Description if the exe has a usable one, else the file name without extension.
pub fn display_name_fallback(desc: Option<&str>, exe_path: &str) -> String {
    if let Some(d) = desc.map(str::trim).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    let file = exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path);
    file.rsplit_once('.')
        .map_or(file, |(stem, _)| stem)
        .to_string()
}

fn file_name_lower(exe_path: &str) -> String {
    exe_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe_path)
        .to_ascii_lowercase()
}

/// Shell surfaces (Start, search, lock screen) that should not rename the menu bar,
/// like Spotlight on macOS leaves the active app name alone.
pub fn is_shell_host(exe_path: &str) -> bool {
    const HOSTS: [&str; 6] = [
        "startmenuexperiencehost.exe",
        "searchhost.exe",
        "shellexperiencehost.exe",
        "shellhost.exe",
        "lockapp.exe",
        "textinputhost.exe",
    ];
    HOSTS.contains(&file_name_lower(exe_path).as_str())
}

/// Name for the menu bar, or `None` to keep the current one.
pub fn display_name(hwnd: HWND) -> Option<String> {
    let (exe, package) = process_identity(hwnd)?;
    if is_shell_host(&exe) {
        return None;
    }
    // Store apps: their exe description is poor ("WhatsApp.Root"), the Start menu name is right.
    // Desktop apps: the exe description is the product name and stays the same for every window.
    if let Some(name) = package.and_then(|id| aumid_name(&id)) {
        return Some(name);
    }
    // ponytail: a Store app frame without its AUMID yet is skipped; the name updates on the next focus change.
    if file_name_lower(&exe) == "applicationframehost.exe" {
        return None;
    }
    Some(display_name_fallback(
        file_description(&exe).as_deref(),
        &exe,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_prefers_description() {
        assert_eq!(
            display_name_fallback(Some("Visual Studio Code"), r"C:\x\Code.exe"),
            "Visual Studio Code"
        );
        assert_eq!(display_name_fallback(Some("   "), r"C:\x\Code.exe"), "Code");
        assert_eq!(
            display_name_fallback(None, r"C:\Program Files\App\my.tool.exe"),
            "my.tool"
        );
        assert_eq!(display_name_fallback(None, "noext"), "noext");
    }

    #[test]
    fn shell_hosts_are_ignored_case_insensitively() {
        assert!(is_shell_host(r"C:\Windows\SystemApps\X\SearchHost.exe"));
        assert!(is_shell_host(
            r"C:\Windows\SystemApps\X\StartMenuExperienceHost.exe"
        ));
        assert!(!is_shell_host(r"C:\Windows\explorer.exe"));
    }

    #[test]
    fn reads_description_of_a_system_exe() {
        let desc = file_description(r"C:\Windows\explorer.exe");
        assert!(desc.is_some_and(|d| !d.is_empty()));
    }

    /// Manual check against real windows: `cargo test names_of_open_windows -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn names_of_open_windows() {
        use windows::Win32::Foundation::{LPARAM, TRUE};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextLengthW, IsWindowVisible,
            WS_EX_TOOLWINDOW,
        };
        unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
            unsafe {
                let tool = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
                if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 && !tool {
                    (*(lp.0 as *mut Vec<HWND>)).push(hwnd);
                }
            }
            TRUE
        }
        unsafe {
            _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        let mut list: Vec<HWND> = Vec::new();
        unsafe { _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize)) };
        for hwnd in list {
            let (exe, package) = process_identity(hwnd).unwrap_or_default();
            let id = package.unwrap_or_default();
            println!("    identity {:?}", window_identity(hwnd));
            println!(
                "{:<22} aumid={:<55} apps={:?} -> {:?}",
                file_name_lower(&exe),
                id,
                aumid_name(&id),
                display_name(hwnd)
            );
        }
    }

    #[test]
    fn source_identity_kinds() {
        assert_eq!(
            source_identity(r"shell:AppsFolder\Pkg_x!App").unwrap(),
            AppIdentity {
                aumid: Some("Pkg_x!App".into()),
                exe: None
            }
        );
        assert_eq!(
            source_identity(r"C:\Apps\Tool.EXE").unwrap(),
            AppIdentity {
                aumid: None,
                exe: Some(r"c:\apps\tool.exe".into())
            }
        );
        assert!(source_identity(r"C:\missing\nothing.lnk").is_none());
    }

    #[test]
    fn same_app_by_aumid_or_exe() {
        let id = |a: Option<&str>, e: Option<&str>| AppIdentity {
            aumid: a.map(Into::into),
            exe: e.map(Into::into),
        };
        let brave_pin = id(Some("Brave"), Some("c:/brave/brave.exe"));
        let brave_profile = id(Some("Brave.UserData.Profile1"), Some("C:/Brave/brave.exe"));
        assert!(brave_pin.same_app(&brave_profile));
        let store = id(Some("App.Pkg!App"), None);
        assert!(store.same_app(&id(Some("app.pkg!app"), None)));
        assert!(!store.same_app(&id(Some("Other!App"), None)));
        assert!(!id(None, None).same_app(&id(None, None)));
    }

    /// Manual check: `cargo test pinned_links_resolve -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pinned_links_resolve() {
        unsafe {
            _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        let dir = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            println!(
                "{:?} -> {:?}",
                e.file_name(),
                link_identity(&e.path().to_string_lossy())
            );
        }
    }
}
