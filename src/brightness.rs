//! Built-in display brightness through WMI (`root\WMI`, the same path the
//! Windows quick settings use for laptop panels). WMI calls take tens of ms,
//! so one worker thread owns the connection; slider moves are coalesced.

use std::cell::OnceCell;
use std::sync::mpsc::{Receiver, Sender, channel};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, Result, w};

enum Job {
    Get(Sender<Option<u8>>),
    Set(u8),
}

thread_local! {
    static WORKER: OnceCell<Sender<Job>> = const { OnceCell::new() };
}

fn send(job: Job) {
    WORKER.with(|w| {
        let tx = w.get_or_init(|| {
            let (tx, rx) = channel();
            std::thread::spawn(move || work(rx));
            tx
        });
        _ = tx.send(job);
    });
}

/// Current brightness 0..=100, `None` without a built-in panel (desktop PCs).
pub fn get() -> Option<u8> {
    let (tx, rx) = channel();
    send(Job::Get(tx));
    rx.recv_timeout(std::time::Duration::from_millis(800)).ok().flatten()
}

pub fn set(percent: u8) {
    send(Job::Set(percent.min(100)));
}

fn work(rx: Receiver<Job>) {
    unsafe { _ = CoInitializeEx(None, COINIT_MULTITHREADED) };
    let svc = connect().inspect_err(|e| crate::log::error(&format!("brightness wmi: {e}"))).ok();
    while let Ok(job) = rx.recv() {
        let mut latest = None;
        let mut handle = |job| match job {
            Job::Get(reply) => _ = reply.send(svc.as_ref().and_then(|s| read(s).ok())),
            Job::Set(v) => latest = Some(v),
        };
        handle(job);
        // A dragged slider queues many values: only the last one matters.
        while let Ok(more) = rx.try_recv() {
            handle(more);
        }
        if let (Some(v), Some(s)) = (latest, svc.as_ref())
            && let Err(e) = write(s, v)
        {
            crate::log::error(&format!("brightness set: {e}"));
        }
    }
}

fn connect() -> Result<IWbemServices> {
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let svc = locator.ConnectServer(&BSTR::from(r"root\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
        CoSetProxyBlanket(&svc, RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE, None, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;
        Ok(svc)
    }
}

fn first(svc: &IWbemServices, query: &str) -> Result<IWbemClassObject> {
    unsafe {
        let en = svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)?;
        let mut out = [None];
        let mut n = 0;
        en.Next(WBEM_INFINITE, &mut out, &mut n).ok()?;
        out[0].take().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
    }
}

fn read(svc: &IWbemServices) -> Result<u8> {
    let obj = first(svc, "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE")?;
    let mut v = VARIANT::default();
    unsafe { obj.Get(w!("CurrentBrightness"), 0, &mut v, None, None)? };
    Ok(i32::try_from(&v)?.clamp(0, 100) as u8)
}

fn write(svc: &IWbemServices, percent: u8) -> Result<()> {
    let target = first(svc, "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active=TRUE")?;
    unsafe {
        let mut path = VARIANT::default();
        target.Get(w!("__PATH"), 0, &mut path, None, None)?;
        let path = BSTR::try_from(&path)?;
        let mut class = None;
        svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, Some(&mut class), None)?;
        let class: IWbemClassObject = class.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
        let mut sig = None;
        class.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, std::ptr::null_mut())?;
        let params = sig.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?.SpawnInstance(0)?;
        params.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0)?;
        params.Put(w!("Brightness"), 0, &VARIANT::from(percent), 0)?;
        svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, &params, None, None)
    }
}

#[cfg(test)]
mod tests {
    /// Reads the real panel (no change): `cargo test brightness -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn reads_brightness() {
        println!("brightness: {:?}", super::get());
    }
}
