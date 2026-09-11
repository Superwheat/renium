//! Start Studio without activating its first window. Rust's stable Command API
//! does not expose STARTUPINFO.wShowWindow (including our Rust 1.89 minimum).
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateProcessW, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESHOWWINDOW, STARTUPINFOW, TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

const LAUNCH_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-launch.dll"));

fn launch_module() -> Result<HMODULE> {
    use sha2::{Digest, Sha256};
    let directory = std::env::temp_dir().join("renium-native");
    std::fs::create_dir_all(&directory)?;
    // One immutable image per build. Configuration lives inside each selected
    // Studio, so simultaneous places and repeated reopens can share the file.
    let path = directory.join(format!(
        "renium-launch-{:x}.dll",
        Sha256::digest(LAUNCH_BYTES)
    ));
    if std::fs::read(&path).ok().as_deref() != Some(LAUNCH_BYTES) {
        crate::system::files::atomic_write_file(&path, LAUNCH_BYTES)?;
    }
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(std::io::Error::last_os_error()).context("Could not load Studio launch guard");
    }
    Ok(module)
}

// Quote each argument using Windows CRT rules. Keeping UTF-16 preserves paths
// containing non-ASCII characters and avoids any shell interpretation.
fn append_argument(output: &mut Vec<u16>, argument: &OsStr) -> Result<()> {
    output.push(b'"' as u16);
    let mut slashes = 0;
    for unit in argument.encode_wide() {
        match unit {
            0 => bail!("Studio launch arguments cannot contain NUL"),
            92 => slashes += 1,
            _ => {
                let count = if unit == 34 { slashes * 2 + 1 } else { slashes };
                output.extend(std::iter::repeat_n(92, count));
                output.push(unit);
                slashes = 0;
            }
        }
    }
    output.extend(std::iter::repeat_n(92, slashes * 2));
    output.push(b'"' as u16);
    Ok(())
}

pub(super) fn spawn(executable: &Path, arguments: &[&OsStr]) -> Result<u32> {
    let mut command_line = Vec::new();
    append_argument(&mut command_line, executable.as_os_str())?;
    for argument in arguments {
        command_line.push(b' ' as u16);
        append_argument(&mut command_line, argument)?;
    }
    command_line.push(0);
    let executable_wide = executable
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: SW_SHOWNOACTIVATE as u16,
        ..unsafe { std::mem::zeroed() }
    };
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // No shell, inherited handles, console, changed environment, or focus
    // restoration. The currently focused app keeps its activation.
    if unsafe {
        CreateProcessW(
            executable_wide.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Err(error).with_context(|| {
            format!(
                "Failed to launch {} in the background",
                executable.display()
            )
        });
    }
    let protection = protect_process(process.dwProcessId);
    if let Err(error) = protection {
        unsafe {
            TerminateProcess(process.hProcess, 1);
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        return Err(error);
    }
    let resumed = unsafe { ResumeThread(process.hThread) };
    let error = std::io::Error::last_os_error();
    unsafe {
        if resumed == u32::MAX {
            TerminateProcess(process.hProcess, 1);
        }
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    if resumed == u32::MAX {
        return Err(error).context("Could not resume Studio");
    }
    Ok(process.dwProcessId)
}

// Load into the exact Studio before resuming its first thread, or before
// requesting Play in an existing Studio. Child process creation is guarded
// inside Studio before the child's first instruction. No desktop hook, input
// queue attachment, or foreground restoration affects another application.
pub(crate) fn protect_process(pid: u32) -> Result<()> {
    let module = launch_module()?;
    let result = (|| -> Result<()> {
        let protect = unsafe { GetProcAddress(module, c"ReniumProtectLaunch".as_ptr().cast()) }
            .context("Studio launch guard omitted process protection")?;
        let protect: unsafe extern "C" fn(u32) -> u32 = unsafe { std::mem::transmute(protect) };
        let error = unsafe { protect(pid) };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error as i32))
                .context("Could not protect Studio activation");
        }
        Ok(())
    })();
    unsafe { FreeLibrary(module) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_launch_blocks_transient_null_foreground_without_blocking_user_input() {
        use std::os::windows::process::CommandExt;
        let output =
            std::process::Command::new(concat!(env!("OUT_DIR"), "/renium-launch-policy-test.exe"))
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .env_remove("RENIUM_TRACE_LAUNCH")
                .output()
                .expect("native launch regression should run without opening any windows");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn studio_launch_preserves_quoted_unicode_and_trailing_slash_arguments() {
        for (argument, expected) in [
            ("", "\"\""),
            (
                "C:\\places\\terrain test.rbxl",
                "\"C:\\places\\terrain test.rbxl\"",
            ),
            ("雪.rbxl", "\"雪.rbxl\""),
            ("a\\\"b\\", "\"a\\\\\\\"b\\\\\""),
        ] {
            let mut output = Vec::new();
            append_argument(&mut output, OsStr::new(argument)).unwrap();
            assert_eq!(String::from_utf16(&output).unwrap(), expected);
        }
        assert!(append_argument(&mut Vec::new(), OsStr::new("bad\0path")).is_err());
    }
}
