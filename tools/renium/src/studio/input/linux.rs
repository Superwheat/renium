use super::StudioWindow;
use anyhow::{Result, bail};

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-input-shield"));

pub struct WindowHandle {
    pid: u32,
    viewport: Option<(i32, i32)>,
}

pub type InputShield = super::unix_shield::InputShield;

pub fn input_shield(handle: &WindowHandle) -> Result<InputShield> {
    let (width, height) = handle.viewport.unwrap_or_default();
    super::unix_shield::start(
        HELPER,
        "Linux",
        [
            handle.pid.to_string(),
            width.to_string(),
            height.to_string(),
            std::process::id().to_string(),
            concat!("Renium ", env!("CARGO_PKG_VERSION")).to_string(),
        ],
    )
}

pub fn window_for_pid(
    pid: u32,
    viewport: Option<(i32, i32)>,
    _restore_minimized: bool,
) -> Result<StudioWindow> {
    if pid == 0 {
        bail!("Studio PID is invalid");
    }
    Ok(StudioWindow {
        label: format!("Studio PID {pid}"),
        handle: WindowHandle { pid, viewport },
    })
}

pub fn capture_window_png(_handle: &WindowHandle, _path: &std::path::Path) -> Result<(u32, u32)> {
    bail!("Window capture is only supported on Windows and macOS")
}

pub fn capture_window_rgba(_handle: &WindowHandle) -> Result<(u32, u32, Vec<u8>)> {
    bail!("Window capture is only supported on Windows and macOS")
}
