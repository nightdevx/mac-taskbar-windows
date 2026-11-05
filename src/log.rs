//! Small file log at %LOCALAPPDATA%\taskbar\log.txt. Startup info and errors only.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 1 << 20;

fn file() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("taskbar");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("log.txt"))
}

fn write(level: &str, msg: &str) {
    #[cfg(debug_assertions)]
    eprintln!("[{level}] {msg}");
    let Some(path) = file() else { return };
    let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES);
    let mut opts = OpenOptions::new();
    opts.create(true);
    if too_big { opts.write(true).truncate(true) } else { opts.append(true) };
    if let Ok(mut f) = opts.open(&path) {
        let t = unsafe { GetLocalTime() };
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{level}] {msg}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        );
    }
}

pub fn info(msg: &str) {
    write("info", msg);
}

pub fn error(msg: &str) {
    write("error", msg);
}

/// Logs panics. Release builds abort after the hook, so this is the last word.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        write("panic", &info.to_string());
    }));
}
