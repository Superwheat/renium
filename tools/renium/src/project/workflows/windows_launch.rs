//! Start Studio without activating its first window. Rust's stable Command API
//! does not expose STARTUPINFO.wShowWindow (including our Rust 1.89 minimum).
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_DLL_INIT_FAILED, FreeLibrary, HMODULE};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList, OpenProcess,
    PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, PROCESS_CREATE_PROCESS, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESHOWWINDOW, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_STYLE, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, SW_SHOWNA, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_MAXIMIZE,
};

const LAUNCH_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-launch.dll"));

fn launch_module() -> Result<HMODULE> {
    use sha2::{Digest, Sha256};
    let directory =
        crate::system::files::expand_short_names(std::env::temp_dir()).join("renium-native");
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
    let (pid, ()) = spawn_prepared(executable, arguments, None, |_| Ok(()))?;
    // Only ordinary Edit launches restore the Edit window's maximized state.
    // Test tasks restore their own instance-specific Studio layout.
    if studio_remembers_maximized() {
        maximize_without_activation(pid);
    }
    Ok(pid)
}

/// Prepare an owned test process before its main thread can load a place.
/// Failure closes that exact process through its retained kernel handle.
pub(crate) fn spawn_prepared<T>(
    executable: &Path,
    arguments: &[&OsStr],
    parent: Option<u32>,
    prepare: impl FnOnce(u32) -> Result<T>,
) -> Result<(u32, T)> {
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
    // Match the child-launch guard for test tasks. Their StudioServer layout
    // must not inherit the ordinary Edit window's maximized state.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESHOWWINDOW;
    startup.StartupInfo.wShowWindow = if parent.is_some() {
        SW_SHOWNOACTIVATE
    } else {
        SW_SHOWNA
    } as u16;
    // Destination tasks remain actual children of the Edit controller, so
    // orphan detection and Stop retain ownership across daemon restarts.
    let parent_handle = parent.map(|pid| unsafe { OpenProcess(PROCESS_CREATE_PROCESS, 0, pid) });
    anyhow::ensure!(
        parent_handle.is_none_or(|handle| !handle.is_null()),
        "The root Edit process ended before server creation"
    );
    let _parent = crate::system::files::OnDrop::new(|| {
        if let Some(handle) = parent_handle {
            unsafe {
                CloseHandle(handle);
            }
        }
    });
    let mut attribute_bytes = 0;
    let mut attribute_storage = Vec::<usize>::new();
    // UpdateProcThreadAttribute retains the address until CreateProcess.
    let mut parent_value = parent_handle.unwrap_or(std::ptr::null_mut());
    if parent.is_some() {
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_bytes);
        }
        attribute_storage.resize(attribute_bytes.div_ceil(std::mem::size_of::<usize>()), 0);
        startup.lpAttributeList = attribute_storage.as_mut_ptr().cast();
        anyhow::ensure!(
            unsafe {
                InitializeProcThreadAttributeList(
                    startup.lpAttributeList,
                    1,
                    0,
                    &mut attribute_bytes,
                )
            } != 0,
            "Cannot initialize destination parent: {}",
            std::io::Error::last_os_error()
        );
        if unsafe {
            UpdateProcThreadAttribute(
                startup.lpAttributeList,
                0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
                (&mut parent_value as *mut windows_sys::Win32::Foundation::HANDLE).cast(),
                std::mem::size_of_val(&parent_value),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            unsafe {
                DeleteProcThreadAttributeList(startup.lpAttributeList);
            }
            return Err(error).context("Cannot retain the root Edit process parent");
        }
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    }
    let _attributes = crate::system::files::OnDrop::new(|| {
        if !startup.lpAttributeList.is_null() {
            unsafe {
                DeleteProcThreadAttributeList(startup.lpAttributeList);
            }
        }
    });
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
            CREATE_NEW_PROCESS_GROUP
                | CREATE_SUSPENDED
                | if parent.is_some() {
                    EXTENDED_STARTUPINFO_PRESENT
                } else {
                    0
                },
            std::ptr::null(),
            std::ptr::null(),
            &startup.StartupInfo,
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
    let prepared =
        match protect_process(process.dwProcessId).and_then(|()| prepare(process.dwProcessId)) {
            Ok(prepared) => prepared,
            Err(error) => {
                unsafe {
                    TerminateProcess(process.hProcess, 1);
                    CloseHandle(process.hThread);
                    CloseHandle(process.hProcess);
                }
                return Err(error);
            }
        };
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
    Ok((process.dwProcessId, prepared))
}

/// Studio saves its main window geometry the Qt way (`@ByteArray(...)` in
/// UTF-16 around a saveGeometry blob). The blob's maximized byte carries
/// Qt::WindowMaximized when the window was maximized. Showing a new window
/// without activation gives it the normal size, so that state is reapplied.
fn studio_remembers_maximized() -> bool {
    let subkey = "Software\\Roblox\\RobloxStudio\\LayoutSettings\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    let value = "window_geometry_ribbon\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != 0 || size == 0 || size > 4096 {
        return false;
    }
    let mut bytes = vec![0u8; size as usize];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            bytes.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return false;
    }
    let prefix = "@ByteArray("
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let geometry = if bytes.starts_with(&prefix) {
        bytes[prefix.len()..]
            .chunks_exact(2)
            .map(|unit| unit[0])
            .collect::<Vec<_>>()
    } else {
        bytes
    };
    geometry.starts_with(&[0x01, 0xD9, 0xD0, 0xCB])
        && geometry.get(44).is_some_and(|flags| flags & 2 != 0)
}

fn maximize_without_activation(pid: u32) {
    struct Search {
        pid: u32,
        found: windows_sys::Win32::Foundation::HWND,
    }
    unsafe extern "system" fn visit(
        window: windows_sys::Win32::Foundation::HWND,
        parameter: windows_sys::Win32::Foundation::LPARAM,
    ) -> windows_sys::Win32::Foundation::BOOL {
        let search = unsafe { &mut *(parameter as *mut Search) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner != search.pid || unsafe { IsWindowVisible(window) } == 0 {
            return 1;
        }
        // The splash window is titled too; the main window ends with the
        // application name after a separator.
        let length = unsafe { GetWindowTextLengthW(window) };
        if length <= 0 {
            return 1;
        }
        let mut title = vec![0u16; length as usize + 1];
        let copied = unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) };
        let title = String::from_utf16_lossy(&title[..copied.max(0) as usize]);
        if title.ends_with("Roblox Studio") && title.contains(" - ") {
            search.found = window;
            return 0;
        }
        1
    }
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(60) {
        let mut search = Search {
            pid,
            found: std::ptr::null_mut(),
        };
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as isize) };
        if !search.found.is_null() {
            // Every show-style maximize activates the window. Setting the
            // maximized style and the work-area frame directly does not.
            let window = search.found;
            let style = unsafe { GetWindowLongPtrW(window, GWL_STYLE) };
            unsafe { SetWindowLongPtrW(window, GWL_STYLE, style | WS_MAXIMIZE as isize) };
            let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
            let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
                let work = info.rcWork;
                unsafe {
                    SetWindowPos(
                        window,
                        std::ptr::null_mut(),
                        work.left,
                        work.top,
                        work.right - work.left,
                        work.bottom - work.top,
                        SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER,
                    )
                };
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
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
        let mut error = 0;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            error = unsafe { protect(pid) };
            if error != ERROR_DLL_INIT_FAILED {
                break;
            }
        }
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error as i32))
                .context("Could not protect Studio activation");
        }
        Ok(())
    })();
    unsafe { FreeLibrary(module) };
    result
}

/// The caller has structurally qualified these fields against the suspended
/// process image. The launch helper uses Qt's own QString allocator.
pub(crate) fn prepare_test_server(
    pid: u32,
    root_pid: u32,
    fields: [u64; 4],
    file: &Path,
) -> Result<()> {
    #[repr(C)]
    struct Input {
        version: u32,
        size: u32,
        fields: [u64; 4],
        root_creation: u64,
        root_pid: u32,
        reserved: u32,
        file: [u16; 32768],
    }
    let mut input = Box::new(Input {
        version: 2,
        size: std::mem::size_of::<Input>() as u32,
        fields,
        root_creation: crate::app::update::process_start_identity(root_pid)
            .context("Root test server ended")?
            .parse()?,
        root_pid,
        reserved: 0,
        file: [0; 32768],
    });
    let path: Vec<_> = file.as_os_str().encode_wide().collect();
    anyhow::ensure!(
        !path.is_empty() && path.len() < input.file.len() && !path.contains(&0),
        "Invalid destination test file path"
    );
    input.file[..path.len()].copy_from_slice(&path);
    let module = launch_module()?;
    let result = (|| -> Result<()> {
        let entry = unsafe { GetProcAddress(module, c"ReniumPrepareTestServer".as_ptr().cast()) }
            .context("Studio launch guard omitted destination loading")?;
        let entry: unsafe extern "C" fn(u32, *const Input) -> u32 =
            unsafe { std::mem::transmute(entry) };
        let error = unsafe { entry(pid, input.as_ref()) };
        anyhow::ensure!(
            error == 0,
            "Could not prepare destination server: {}",
            std::io::Error::from_raw_os_error(error as i32)
        );
        Ok(())
    })();
    unsafe { FreeLibrary(module) };
    result
}

#[repr(C)]
struct TestRouteInput {
    version: u32,
    size: u32,
    action: u32,
    client_pid: u32,
    client_creation: u64,
    window: u64,
    token: u64,
    state: u32,
    error: u32,
    command: [u16; 32768],
}

pub(crate) struct TestClientRoute {
    pid: u32,
    input: Box<TestRouteInput>,
}
impl TestClientRoute {
    pub(crate) fn arm(server: u32, client: u32) -> Result<Self> {
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot create client route identity: {e}"))?;
        let mut route = Self {
            pid: server,
            input: Box::new(TestRouteInput {
                version: 1,
                size: std::mem::size_of::<TestRouteInput>() as u32,
                action: 0,
                client_pid: client,
                client_creation: crate::app::update::process_start_identity(client)
                    .context("Teleport client ended")?
                    .parse()?,
                window: crate::studio::input::teleport_caption_window(client)?.0 as u64,
                token: u64::from_le_bytes(token).max(1),
                state: 0,
                error: 0,
                command: [0; 32768],
            }),
        };
        route.invoke(1)?;
        Ok(route)
    }
    fn invoke(&mut self, action: u32) -> Result<()> {
        self.input.action = action;
        let module = launch_module()?;
        let result = (|| -> Result<()> {
            let entry = unsafe { GetProcAddress(module, c"ReniumTestRoute".as_ptr().cast()) }
                .context("Studio launch helper omitted client reuse")?;
            let entry: unsafe extern "C" fn(u32, *mut TestRouteInput) -> u32 =
                unsafe { std::mem::transmute(entry) };
            let error = unsafe { entry(self.pid, self.input.as_mut()) };
            anyhow::ensure!(
                error == 0,
                "Cannot bind destination client reuse: {}",
                std::io::Error::from_raw_os_error(error as i32)
            );
            Ok(())
        })();
        unsafe { FreeLibrary(module) };
        result
    }
    pub(crate) fn command(&mut self) -> Result<Option<String>> {
        self.invoke(2)?;
        anyhow::ensure!(
            self.input.state != 3,
            "The client process ended before its destination join"
        );
        if self.input.state != 2 {
            return Ok(None);
        }
        let length = self
            .input
            .command
            .iter()
            .position(|c| *c == 0)
            .context("Unterminated client route command")?;
        Ok(Some(String::from_utf16(&self.input.command[..length])?))
    }
}
impl Drop for TestClientRoute {
    fn drop(&mut self) {
        let _ = self.invoke(3);
    }
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
