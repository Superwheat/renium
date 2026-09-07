//! Bounded subprocess execution shared by the host and SDK. No shell or global input.
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 32 * 1024 * 1024;

/// A permission/query failure is not evidence that a process has ended.
pub fn alive(pid: u32) -> Result<bool> {
    if pid == 0 {
        bail!("PID must be positive");
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(false);
            }
            return Err(error.into());
        }
        let state = WaitForSingleObject(handle, 0);
        let error = std::io::Error::last_os_error();
        CloseHandle(handle);
        match state {
            0 => Ok(false),
            258 => Ok(true),
            _ => Err(error.into()),
        }
    }
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            bail!("PID is out of range");
        }
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error.into()),
        }
    }
}

struct ChildGuard {
    child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ChildGuard {
    fn spawn(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let child = command.spawn().context("Could not start plugin command")?;
        #[allow(unused_mut)] // Windows attaches the job after creating the child.
        let mut guard = Self {
            child,
            #[cfg(windows)]
            job: std::ptr::null_mut(),
        };
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            unsafe {
                guard.job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if guard.job.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags =
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                if SetInformationJobObject(
                    guard.job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                    || AssignProcessToJobObject(guard.job, guard.child.as_raw_handle() as _) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
        }
        Ok(guard)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            if !self.job.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn output(command: Command, input: &[u8], timeout: Duration) -> Result<Output> {
    let mut guard = ChildGuard::spawn(command)?;
    let deadline = Instant::now() + timeout;
    let mut stdin = guard.child.stdin.take().context("Missing command stdin")?;
    let stdout = guard
        .child
        .stdout
        .take()
        .context("Missing command stdout")?;
    let stderr = guard
        .child
        .stderr
        .take()
        .context("Missing command stderr")?;
    let input = input.to_vec();
    let (tx, rx) = mpsc::channel();
    let writer = tx.clone();
    thread::spawn(move || {
        let _ = writer.send((2, stdin.write_all(&input).map(|_| Vec::new())));
    });
    for (id, pipe) in [
        (0, Box::new(stdout) as Box<dyn Read + Send>),
        (1, Box::new(stderr) as Box<dyn Read + Send>),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(MAX_OUTPUT + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send((id, result));
        });
    }
    drop(tx);
    let mut buffers = [None, None, None];
    let status = loop {
        for (id, result) in rx.try_iter() {
            let bytes = result.context("Plugin pipe failed")?;
            if bytes.len() as u64 > MAX_OUTPUT {
                bail!("Plugin command output exceeded 32 MiB");
            }
            buffers[id] = Some(bytes);
        }
        if buffers.iter().all(Option::is_some)
            && let Some(status) = guard.child.try_wait()?
        {
            break status;
        }
        if Instant::now() >= deadline {
            bail!(
                "Plugin command exceeded {:.1}s; its process tree was stopped",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    Ok(Output {
        status,
        stdout: buffers[0].take().unwrap(),
        stderr: buffers[1].take().unwrap(),
    })
}
