//! Who owns a window: exe path, package AppUserModelID and a human display name.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HWND};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
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
    let exe = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }
        .ok()
        .map(|_| String::from_utf16_lossy(&buf[..len as usize]));
    let mut len = buf.len() as u32;
    let package = (unsafe { GetApplicationUserModelId(process, &mut len, Some(PWSTR(buf.as_mut_ptr()))) } == ERROR_SUCCESS)
        .then(|| String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]));
    unsafe { _ = CloseHandle(process) };
    Some((exe?, package))
}

/// Start menu name for an AUMID ("Not Defteri", "Google Chrome").
pub fn aumid_name(aumid: &str) -> Option<String> {
    if aumid.is_empty() {
        return None; // would name the Apps folder itself
    }
    unsafe {
        let path = HSTRING::from(format!(r"shell:AppsFolder\{aumid}"));
        let item: IShellItem = SHCreateItemFromParsingName(&path, None).ok()?;
        take_pwstr(item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?)
    }
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
            VerQueryValueW(data.as_ptr() as *const c_void, &HSTRING::from(key), &mut p, &mut len)
                .as_bool()
                .then_some((p as *const c_void, len))
        };
        // Language/codepage pairs the file declares, then the usual English ones.
        let mut langs: Vec<(u16, u16)> = query(r"\VarFileInfo\Translation")
            .map(|(p, len)| std::slice::from_raw_parts(p as *const (u16, u16), len as usize / 4).to_vec())
            .unwrap_or_default();
        langs.extend([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
        langs.into_iter().find_map(|(lang, cp)| {
            let (p, len) = query(&format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription"))?;
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p as *const u16, len as usize));
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
    file.rsplit_once('.').map_or(file, |(stem, _)| stem).to_string()
}

fn file_name_lower(exe_path: &str) -> String {
    exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path).to_ascii_lowercase()
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
    Some(display_name_fallback(file_description(&exe).as_deref(), &exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_prefers_description() {
        assert_eq!(display_name_fallback(Some("Visual Studio Code"), r"C:\x\Code.exe"), "Visual Studio Code");
        assert_eq!(display_name_fallback(Some("   "), r"C:\x\Code.exe"), "Code");
        assert_eq!(display_name_fallback(None, r"C:\Program Files\App\my.tool.exe"), "my.tool");
        assert_eq!(display_name_fallback(None, "noext"), "noext");
    }

    #[test]
    fn shell_hosts_are_ignored_case_insensitively() {
        assert!(is_shell_host(r"C:\Windows\SystemApps\X\SearchHost.exe"));
        assert!(is_shell_host(r"C:\Windows\SystemApps\X\StartMenuExperienceHost.exe"));
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
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextLengthW, IsWindowVisible, WS_EX_TOOLWINDOW};
        unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
            unsafe {
                let tool = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
                if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 && !tool {
                    (*(lp.0 as *mut Vec<HWND>)).push(hwnd);
                }
            }
            TRUE
        }
        unsafe { _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
        let mut list: Vec<HWND> = Vec::new();
        unsafe { _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize)) };
        for hwnd in list {
            let (exe, package) = process_identity(hwnd).unwrap_or_default();
            let id = package.unwrap_or_default();
            println!("{:<22} aumid={:<55} apps={:?} -> {:?}", file_name_lower(&exe), id, aumid_name(&id), display_name(hwnd));
        }
    }
}
