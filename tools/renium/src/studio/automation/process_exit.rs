//! Retain exit notifications before asking Studio to close its test processes.
//! A reused PID must never extend a previous test's shutdown.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

// BindToClose gets 30 seconds; allow a further 10 for Studio application teardown.
// This is one deadline for the whole stop, not a delay per process.
pub(super) const STOP_TIMEOUT: Duration = Duration::from_secs(40);

pub(super) struct ProcessExit {
    pid: u32,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(target_os = "macos")]
    queue: std::os::fd::OwnedFd,
}

impl ProcessExit {
    #[cfg(windows)]
    pub(super) fn watch(pid: u32) -> Result<Option<Self>> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

        // SAFETY: OpenProcess returns a new owned handle, without modifying the process.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(None); // Already exited before the stop request.
            }
            return Err(error).with_context(|| format!("Cannot observe Studio process {pid}"));
        }
        Ok(Some(Self {
            pid,
            // SAFETY: the valid handle has no other owner.
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
        }))
    }

    #[cfg(windows)]
    fn exited_by(&self, deadline: Instant) -> Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: self retains the synchronization handle throughout the wait.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), millis) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error()).context("Cannot wait for Studio exit"),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn watch(pid: u32) -> Result<Option<Self>> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // SAFETY: kqueue returns a new descriptor, transferred to OwnedFd below.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("Cannot observe Studio exit");
        }
        // SAFETY: fd is valid and has no other owner.
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        let event = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: registers exactly one event; no output buffer is requested.
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("Cannot observe Studio process {pid}"));
        }
        Ok(Some(Self { pid, queue }))
    }

    #[cfg(target_os = "macos")]
    fn exited_by(&self, deadline: Instant) -> Result<bool> {
        use std::os::fd::AsRawFd;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = libc::timespec {
                tv_sec: remaining.as_secs() as libc::time_t,
                tv_nsec: remaining.subsec_nanos() as libc::c_long,
            };
            let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
            // SAFETY: the queue is retained; one event fits in the output buffer.
            let count = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    event.as_mut_ptr(),
                    1,
                    &timeout,
                )
            };
            if count >= 0 {
                return Ok(count == 1);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("Cannot wait for Studio exit");
            }
        }
    }
}

pub(super) fn wait(processes: &[ProcessExit], deadline: Instant) -> Result<()> {
    for process in processes {
        if !process.exited_by(deadline)? {
            bail!(
                "Studio test process {} did not exit within the 40-second shutdown budget (including BindToClose); inspect its shutdown callbacks before starting another test",
                process.pid
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn retained_exit_wait_is_bounded_and_observes_the_original_process() {
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("cmd");
            command.args(["/d", "/c", "pause"]);
            command.creation_flags(0x08000000); // No console window or input interaction.
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("cat");
            command.stdout(Stdio::null());
            command
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let watched = ProcessExit::watch(child.id()).unwrap().unwrap();
        let timed_out = watched.exited_by(Instant::now() + Duration::from_millis(20));
        // Always reap this owned helper, even if the timeout assertion fails.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!timed_out.unwrap());
        assert!(
            watched
                .exited_by(Instant::now() + Duration::from_secs(1))
                .unwrap()
        );
    }
}
