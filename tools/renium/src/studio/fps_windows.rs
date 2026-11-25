use anyhow::Result;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::{Action, FpsBackend, Status};
use crate::studio::native::serializer::{FpsController, fps_helper_loaded};

pub(super) struct Backend {
    pid: u32,
    process: HANDLE,
    controller: Option<FpsController>,
}

impl Backend {
    pub(super) fn new(pid: u32) -> Result<Self> {
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        anyhow::ensure!(
            !process.is_null(),
            "Could not retain Studio process identity: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            pid,
            process,
            controller: None,
        })
    }

    fn is_focused(&self) -> bool {
        let mut fg_pid = 0;
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), &mut fg_pid);
        }
        fg_pid == self.pid
    }
}

impl FpsBackend for Backend {
    fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    fn step(&mut self, action: Action) -> Result<Status> {
        let allow_load_new = action == Action::Unlock;
        if self.controller.is_none()
            && (allow_load_new || fps_helper_loaded(self.pid).unwrap_or(false))
        {
            self.controller = Some(FpsController::open(self.pid, allow_load_new)?);
        }
        let Some(controller) = &mut self.controller else {
            return Ok(Status {
                unlocked: false,
                focused: self.is_focused(),
                ..Status::default()
            });
        };
        let state = controller.step(action)?;
        Ok(Status {
            unlocked: state.unlocked,
            focused: self.is_focused(),
            hook_installed: state.hook_installed,
            throttle_count: state.last_throttle_count,
            render_checks: state.render_checks,
            unthrottled_frames: state.unthrottled_frames,
            ..Status::default()
        })
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.controller.take();
        unsafe {
            CloseHandle(self.process);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_on_unattached_process_does_not_load_helper() {
        let pid = std::process::id();
        assert!(!fps_helper_loaded(pid).unwrap());
        let mut backend = Backend::new(pid).unwrap();
        let status = backend.step(Action::Status).unwrap();
        assert!(!status.unlocked);
        assert!(!status.hook_installed);
        assert!(backend.controller.is_none());
        assert!(!fps_helper_loaded(pid).unwrap());
    }
}
