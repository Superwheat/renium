//! Keep a background dialog and its owners out of Windows' activation fallback
//! while the dialog closes. Never activate another app or change window z-order.
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{GetLastError, HWND, SetLastError};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GW_OWNER, GWL_EXSTYLE, GetForegroundWindow, GetWindow, GetWindowLongPtrW,
    GetWindowThreadProcessId, SetWindowLongPtrW, WS_EX_NOACTIVATE,
};

struct Lease {
    users: usize,
    added: bool,
}

fn leases() -> &'static Mutex<HashMap<(isize, u32), Lease>> {
    static LEASES: OnceLock<Mutex<HashMap<(isize, u32), Lease>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn process_id(window: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    pid
}

fn set_style(window: HWND, style: isize) -> Result<()> {
    unsafe { SetLastError(0) };
    if unsafe { SetWindowLongPtrW(window, GWL_EXSTYLE, style) } == 0 {
        let error = unsafe { GetLastError() };
        if error != 0 {
            bail!("Could not protect background dialog activation (Windows error {error})");
        }
    }
    Ok(())
}

pub(super) struct BackgroundDialogGuard(Vec<(isize, u32)>);

impl BackgroundDialogGuard {
    pub(super) fn new(mut window: HWND, pid: u32) -> Result<Self> {
        let mut guard = Self(Vec::new());
        if process_id(unsafe { GetForegroundWindow() }) == pid {
            return Ok(guard);
        }
        while !window.is_null() && process_id(window) == pid {
            let key = (window as isize, pid);
            if guard.0.contains(&key) {
                break;
            }
            {
                let mut leases = leases()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let std::collections::hash_map::Entry::Vacant(entry) = leases.entry(key) {
                    let style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) };
                    let added = style & WS_EX_NOACTIVATE as isize == 0;
                    if added {
                        set_style(window, style | WS_EX_NOACTIVATE as isize)?;
                    }
                    entry.insert(Lease { users: 0, added });
                }
                leases.get_mut(&key).unwrap().users += 1;
            }
            guard.0.push(key);
            window = unsafe { GetWindow(window, GW_OWNER) };
        }
        Ok(guard)
    }
}

impl Drop for BackgroundDialogGuard {
    fn drop(&mut self) {
        let mut leases = leases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &self.0 {
            let Some(lease) = leases.get_mut(key) else {
                continue;
            };
            lease.users -= 1;
            if lease.users != 0 {
                continue;
            }
            let window = key.0 as HWND;
            if lease.added && process_id(window) == key.1 {
                let style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) };
                if let Err(error) = set_style(window, style & !(WS_EX_NOACTIVATE as isize)) {
                    crate::app::output::log_global(2, format_args!("[renium] {error:#}"));
                }
            }
            leases.remove(key);
        }
    }
}

#[cfg(test)]
#[test]
fn overlapping_dialog_guards_restore_only_their_own_style_bit() {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            windows_sys::w!("STATIC"),
            windows_sys::w!("Renium dialog guard test"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(!window.is_null());
    let pid = unsafe { GetCurrentProcessId() };
    let first = BackgroundDialogGuard::new(window, pid).unwrap();
    let second = BackgroundDialogGuard::new(window, pid).unwrap();
    drop(first);
    assert_ne!(
        unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } & WS_EX_NOACTIVATE as isize,
        0
    );
    drop(second);
    assert_eq!(
        unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) },
        WS_EX_TOOLWINDOW as isize
    );
    set_style(window, (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize).unwrap();
    drop(BackgroundDialogGuard::new(window, pid).unwrap());
    assert_eq!(
        unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) },
        (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize
    );
    unsafe { DestroyWindow(window) };
}
