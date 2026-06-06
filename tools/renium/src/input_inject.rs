//! Focus-free input injection into Roblox Studio play-test windows.
//!
//! Windows: posts WM_MOUSE*/WM_KEY* messages to the largest child window (the
//! 3D viewport) of each Studio top-level window; the target window never needs
//! focus. macOS: posts CGEvents to the Studio process id, with locations
//! computed from the window's on-screen bounds; requires the Accessibility
//! permission. Both backends expose window-relative coordinates; the caller
//! calibrates a viewport offset by posting a mouse move and asking the target
//! client (over the plugin bridge) where the engine saw the cursor.

use anyhow::{bail, Result};

pub struct StudioWindow {
    pub label: String,
    handle: platform::WindowHandle,
    #[cfg(windows)]
    _reminimize: Option<platform::ReminimizeGuard>,
}

#[cfg(not(windows))]
pub fn list_studio_windows() -> Result<Vec<StudioWindow>> {
    platform::list_studio_windows()
}

#[cfg(windows)]
pub fn window_for_pid(pid: u32, viewport: Option<(i32, i32)>) -> Result<StudioWindow> {
    platform::window_for_pid(pid, viewport)
}

#[cfg(windows)]
pub fn pid_for_local_tcp_port(port: u16) -> Result<u32> {
    platform::pid_for_local_tcp_port(port)
}

#[cfg(not(windows))]
pub fn post_mouse_move(window: &StudioWindow, x: i32, y: i32) -> Result<()> {
    platform::post_mouse_move(&window.handle, x, y)
}

pub fn post_probe_click(window: &StudioWindow, x: i32, y: i32) -> Result<()> {
    platform::post_probe_click(&window.handle, x, y)
}

pub fn post_text(window: &StudioWindow, text: &str) -> Result<()> {
    platform::post_text(&window.handle, text)
}

pub fn capture_window_png(window: &StudioWindow, path: &std::path::Path) -> Result<(u32, u32)> {
    platform::capture_window_png(&window.handle, path)
}

pub fn post_mouse_click(
    window: &StudioWindow,
    x: i32,
    y: i32,
    right: bool,
    hold_ms: u64,
) -> Result<()> {
    platform::post_mouse_click(&window.handle, x, y, right, hold_ms)
}

pub fn post_key(window: &StudioWindow, key: &KeySpec, hold_ms: u64) -> Result<()> {
    platform::post_key(&window.handle, key, hold_ms)
}

pub struct KeySpec {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub windows_vk: u16,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub mac_keycode: u16,
    pub name: &'static str,
}

pub fn resolve_key(name: &str) -> Result<KeySpec> {
    let trimmed = name.trim();
    let upper = trimmed.to_ascii_uppercase();
    let entry: Option<(u16, u16, &'static str)> = if upper.len() == 1 {
        let ch = upper.as_bytes()[0];
        match ch {
            b'A'..=b'Z' => Some((ch as u16, mac_letter_keycode(ch), letter_name(ch))),
            b'0'..=b'9' => Some((ch as u16, mac_digit_keycode(ch), digit_name(ch))),
            _ => None,
        }
    } else {
        match upper.as_str() {
            "SPACE" => Some((0x20, 49, "Space")),
            "ENTER" | "RETURN" => Some((0x0D, 36, "Return")),
            "ESCAPE" | "ESC" => Some((0x1B, 53, "Escape")),
            "TAB" => Some((0x09, 48, "Tab")),
            "BACKSPACE" => Some((0x08, 51, "Backspace")),
            "UP" => Some((0x26, 126, "Up")),
            "DOWN" => Some((0x28, 125, "Down")),
            "LEFT" => Some((0x25, 123, "Left")),
            "RIGHT" => Some((0x27, 124, "Right")),
            "SHIFT" | "LEFTSHIFT" => Some((0xA0, 56, "LeftShift")),
            "CTRL" | "CONTROL" | "LEFTCONTROL" => Some((0xA2, 59, "LeftControl")),
            "ALT" | "LEFTALT" => Some((0xA4, 58, "LeftAlt")),
            _ => None,
        }
    };
    match entry {
        Some((vk, mac, canonical)) => Ok(KeySpec {
            windows_vk: vk,
            mac_keycode: mac,
            name: canonical,
        }),
        None => bail!(
            "Unsupported key '{trimmed}'. Use A-Z, 0-9, Space, Enter, Escape, Tab, Backspace, \
             Up, Down, Left, Right, Shift, Ctrl, or Alt."
        ),
    }
}

fn letter_name(ch: u8) -> &'static str {
    const NAMES: [&str; 26] = [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
        "S", "T", "U", "V", "W", "X", "Y", "Z",
    ];
    NAMES[(ch - b'A') as usize]
}

fn digit_name(ch: u8) -> &'static str {
    const NAMES: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    NAMES[(ch - b'0') as usize]
}

fn mac_letter_keycode(ch: u8) -> u16 {
    // ANSI virtual keycodes are not alphabetical; explicit table.
    const CODES: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    CODES[(ch - b'A') as usize]
}

fn mac_digit_keycode(ch: u8) -> u16 {
    const CODES: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    CODES[(ch - b'0') as usize]
}

#[cfg_attr(not(windows), allow(dead_code))]
fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    use anyhow::Context;
    let file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_filter(png::FilterType::Paeth);
    let _ = encoder.add_text_chunk("Software".to_string(), "capture-rs 0.5.3+build.57".to_string());
    let mut writer = encoder.write_header().context("Failed to write PNG header")?;
    writer
        .write_image_data(rgba)
        .context("Failed to write PNG data")?;
    Ok(())
}

#[cfg(windows)]
mod platform {
    use super::StudioWindow;
    use anyhow::{bail, Context, Result};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GetClientRect, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, PostMessageW, ShowWindow, SW_SHOWMINNOACTIVE,
        SW_SHOWNOACTIVATE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
        WM_RBUTTONDOWN, WM_RBUTTONUP,
    };

    const MK_LBUTTON: WPARAM = 0x0001;
    const MK_RBUTTON: WPARAM = 0x0002;

    #[derive(Clone)]
    pub struct WindowHandle {
        viewport: isize,
        capture: isize,
        render_matched: bool,
        offset_x: i32,
        offset_y: i32,
    }

    struct EnumTopState {
        pids: Vec<u32>,
        windows: Vec<(isize, u32, String)>,
    }

    unsafe extern "system" fn enum_top_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut EnumTopState) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if !state.pids.contains(&pid) {
            return 1;
        }
        let mut title = [0u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        if len <= 0 {
            return 1;
        }
        let text = String::from_utf16_lossy(&title[..len as usize]);
        state.windows.push((hwnd as isize, pid, text));
        1
    }

    struct EnumChildState {
        best: isize,
        best_area: i64,
        best_width: i32,
        best_height: i32,
        exact: isize,
        render: isize,
        render_area: i64,
        viewport_width: i32,
        viewport_height: i32,
    }

    const DISPLAY_SCALES: [f64; 9] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0];

    fn matches_scaled_viewport(width: i32, height: i32, viewport_w: i32, viewport_h: i32) -> bool {
        if viewport_w <= 0 || viewport_h <= 0 {
            return false;
        }
        DISPLAY_SCALES.iter().any(|scale| {
            (width as f64 - viewport_w as f64 * scale).abs() <= 2.0
                && (height as f64 - viewport_h as f64 * scale).abs() <= 2.0
        })
    }

    unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut EnumChildState) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
            return 1;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width == state.viewport_width && height == state.viewport_height {
            state.exact = hwnd as isize;
        }
        if matches_scaled_viewport(width, height, state.viewport_width, state.viewport_height) {
            let area = width as i64 * height as i64;
            if state.render == 0 || area <= state.render_area {
                state.render = hwnd as isize;
                state.render_area = area;
            }
        }
        let area = width as i64 * height as i64;
        if area > state.best_area {
            state.best_area = area;
            state.best = hwnd as isize;
            state.best_width = width;
            state.best_height = height;
        }
        1
    }

    fn viewport_child(top: isize, viewport: Option<(i32, i32)>) -> WindowHandle {
        let (viewport_width, viewport_height) = viewport.unwrap_or((-1, -1));
        let mut state = EnumChildState {
            best: top,
            best_area: 0,
            best_width: 0,
            best_height: 0,
            exact: 0,
            render: 0,
            render_area: 0,
            viewport_width,
            viewport_height,
        };
        {
            let _dpi = ThreadDpiAwareness::per_monitor_v2();
            unsafe {
                EnumChildWindows(
                    top as HWND,
                    Some(enum_child_proc),
                    &mut state as *mut EnumChildState as LPARAM,
                );
            }
        }
        let click_target = if state.exact != 0 { state.exact } else { state.best };
        let capture = if state.render != 0 { state.render } else { click_target };
        WindowHandle {
            viewport: click_target,
            capture,
            render_matched: state.render != 0,
            offset_x: 0,
            offset_y: 0,
        }
    }

    pub fn pid_for_local_tcp_port(port: u16) -> Result<u32> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
            TCP_TABLE_OWNER_PID_ALL,
        };
        const AF_INET: u32 = 2;
        unsafe {
            let mut size = 0u32;
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if size == 0 {
                bail!("GetExtendedTcpTable returned no table size");
            }
            let mut buffer = vec![0u8; size as usize];
            let status = GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut _,
                &mut size,
                0,
                AF_INET,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if status != 0 {
                bail!("GetExtendedTcpTable failed with status {status}");
            }
            let table = buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
            let count = (*table).dwNumEntries as usize;
            let rows = (*table).table.as_ptr();
            for index in 0..count {
                let row: &MIB_TCPROW_OWNER_PID = &*rows.add(index);
                let local_port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
                if local_port == port {
                    return Ok(row.dwOwningPid);
                }
            }
        }
        bail!("No TCP connection with local port {port} found")
    }

    struct ThreadDpiAwareness {
        previous: isize,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetThreadDpiAwarenessContext(value: isize) -> isize;
    }

    impl ThreadDpiAwareness {
        fn per_monitor_v2() -> Self {
            const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
            let previous =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            Self { previous }
        }
    }

    impl Drop for ThreadDpiAwareness {
        fn drop(&mut self) {
            if self.previous != 0 {
                unsafe { SetThreadDpiAwarenessContext(self.previous) };
            }
        }
    }

    pub struct ReminimizeGuard {
        top: isize,
    }

    impl Drop for ReminimizeGuard {
        fn drop(&mut self) {
            unsafe { ShowWindow(self.top as HWND, SW_SHOWMINNOACTIVE) };
        }
    }

    pub fn window_for_pid(pid: u32, viewport: Option<(i32, i32)>) -> Result<StudioWindow> {
        let mut state = EnumTopState {
            pids: vec![pid],
            windows: Vec::new(),
        };
        unsafe {
            EnumWindows(
                Some(enum_top_proc),
                &mut state as *mut EnumTopState as LPARAM,
            );
        }
        let (hwnd, pid, title) = state
            .windows
            .into_iter()
            .next()
            .with_context(|| format!("No visible window found for Studio process {pid}"))?;
        let reminimize = if unsafe { IsIconic(hwnd as HWND) } != 0 {
            unsafe { ShowWindow(hwnd as HWND, SW_SHOWNOACTIVATE) };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(150));
                let probe = viewport_child(hwnd, viewport);
                if probe.render_matched || std::time::Instant::now() >= deadline {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            Some(ReminimizeGuard { top: hwnd })
        } else {
            None
        };
        Ok(StudioWindow {
            label: format!("pid {pid}: {title}"),
            handle: viewport_child(hwnd, viewport),
            _reminimize: reminimize,
        })
    }

    fn mouse_lparam(handle: &WindowHandle, x: i32, y: i32) -> LPARAM {
        let x = x + handle.offset_x;
        let y = y + handle.offset_y;
        (((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32 as LPARAM
    }

    fn post(hwnd: isize, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<()> {
        let ok = unsafe { PostMessageW(hwnd as HWND, msg, wparam, lparam) };
        if ok == 0 {
            bail!("PostMessageW failed for message {msg:#x}");
        }
        Ok(())
    }

    pub fn post_probe_click(handle: &WindowHandle, x: i32, y: i32) -> Result<()> {
        const WM_MBUTTONDOWN: u32 = 0x0207;
        const WM_MBUTTONUP: u32 = 0x0208;
        const MK_MBUTTON: WPARAM = 0x0010;
        let lparam = mouse_lparam(handle, x, y);
        post(handle.viewport, WM_MBUTTONDOWN, MK_MBUTTON, lparam)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        post(handle.viewport, WM_MBUTTONUP, 0, lparam)
    }

    pub fn post_mouse_click(
        handle: &WindowHandle,
        x: i32,
        y: i32,
        right: bool,
        hold_ms: u64,
    ) -> Result<()> {
        let lparam = mouse_lparam(handle, x, y);
        let hold = std::time::Duration::from_millis(hold_ms.clamp(10, 2000));
        post(handle.viewport, WM_MOUSEMOVE, 0, lparam)?;
        if right {
            post(handle.viewport, WM_RBUTTONDOWN, MK_RBUTTON, lparam)?;
            std::thread::sleep(hold);
            post(handle.viewport, WM_RBUTTONUP, 0, lparam)
        } else {
            post(handle.viewport, WM_LBUTTONDOWN, MK_LBUTTON, lparam)?;
            std::thread::sleep(hold);
            post(handle.viewport, WM_LBUTTONUP, 0, lparam)
        }
    }

    pub fn post_key(handle: &WindowHandle, key: &super::KeySpec, hold_ms: u64) -> Result<()> {
        let vk = key.windows_vk as usize;
        let scan = unsafe { MapVirtualKeyW(key.windows_vk as u32, MAPVK_VK_TO_VSC) } as usize;
        let down_lparam = (1usize | (scan << 16)) as isize as LPARAM;
        let up_lparam = (1usize | (scan << 16) | (1 << 30) | (1 << 31)) as isize as LPARAM;
        post(handle.viewport, WM_KEYDOWN, vk as WPARAM, down_lparam)?;
        std::thread::sleep(std::time::Duration::from_millis(hold_ms.clamp(10, 2000)));
        post(handle.viewport, WM_KEYUP, vk as WPARAM, up_lparam)
    }

    pub fn post_text(handle: &WindowHandle, text: &str) -> Result<()> {
        const WM_CHAR: u32 = 0x0102;
        for unit in text.encode_utf16() {
            post(handle.viewport, WM_CHAR, unit as WPARAM, 1)?;
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Ok(())
    }

    pub fn capture_window_png(handle: &WindowHandle, path: &std::path::Path) -> Result<(u32, u32)> {
        use windows_sys::Win32::Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
            GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS, HDC, SRCCOPY,
        };
        #[link(name = "user32")]
        unsafe extern "system" {
            fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
        }
        const PW_CLIENTONLY: u32 = 0x1;
        const PW_RENDERFULLCONTENT: u32 = 0x2;

        let _dpi = ThreadDpiAwareness::per_monitor_v2();
        let hwnd = handle.capture as HWND;
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
            bail!("GetClientRect failed for capture target");
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            bail!("Capture target has no client area");
        }

        unsafe {
            let window_dc = GetDC(hwnd);
            if window_dc.is_null() {
                bail!("GetDC failed for capture target");
            }
            let memory_dc = CreateCompatibleDC(window_dc);
            let bitmap = CreateCompatibleBitmap(window_dc, width, height);
            let previous = SelectObject(memory_dc, bitmap as _);

            let printed = PrintWindow(hwnd, memory_dc, PW_CLIENTONLY | PW_RENDERFULLCONTENT);
            if printed == 0 {
                BitBlt(memory_dc, 0, 0, width, height, window_dc, 0, 0, SRCCOPY);
            }

            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
            let copied = GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr() as *mut _,
                &mut info,
                DIB_RGB_COLORS,
            );

            SelectObject(memory_dc, previous);
            DeleteObject(bitmap as _);
            DeleteDC(memory_dc);
            ReleaseDC(hwnd, window_dc);

            if copied == 0 {
                bail!("GetDIBits failed for capture target");
            }
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 0xFF;
            }
            super::write_png(path, width as u32, height as u32, &pixels)?;
        }
        Ok((width as u32, height as u32))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::StudioWindow;
    use anyhow::{bail, Result};
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CGEventRef = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
    const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    const K_CF_NUMBER_INT_TYPE: isize = 9;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    const EVENT_MOUSE_MOVED: u32 = 5;
    const EVENT_LEFT_DOWN: u32 = 1;
    const EVENT_LEFT_UP: u32 = 2;
    const EVENT_RIGHT_DOWN: u32 = 3;
    const EVENT_RIGHT_UP: u32 = 4;
    const BUTTON_LEFT: u32 = 0;
    const BUTTON_RIGHT: u32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
        fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect)
            -> bool;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            event_type: u32,
            location: CGPoint,
            button: u32,
        ) -> CGEventRef;
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            keycode: u16,
            key_down: bool,
        ) -> CGEventRef;
        fn CGEventPostToPid(pid: i32, event: CGEventRef);
        fn CGEventKeyboardSetUnicodeString(
            event: CGEventRef,
            length: usize,
            string: *const u16,
        );
        fn CGWindowListCreateImage(
            screen_bounds: CGRect,
            options: u32,
            window_id: u32,
            image_options: u32,
        ) -> *mut c_void;
        fn CGImageGetWidth(image: *const c_void) -> usize;
        fn CGImageGetHeight(image: *const c_void) -> usize;
        fn CGImageRelease(image: *mut c_void);
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageDestinationCreateWithURL(
            url: CFTypeRef,
            type_identifier: CFStringRef,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(
            destination: *mut c_void,
            image: *const c_void,
            properties: *const c_void,
        );
        fn CGImageDestinationFinalize(destination: *mut c_void) -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const c_void,
            buffer: *const u8,
            buffer_length: isize,
            is_directory: bool,
        ) -> CFTypeRef;
        fn CFArrayGetCount(array: CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFStringRef) -> CFTypeRef;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const u8,
            encoding: u32,
        ) -> CFStringRef;
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut u8,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
        fn CFNumberGetValue(number: CFNumberRef, number_type: isize, value: *mut c_void) -> bool;
        fn CFRelease(value: CFTypeRef);
    }

    #[derive(Clone)]
    pub struct WindowHandle {
        pid: i32,
        window_number: u32,
        origin_x: f64,
        origin_y: f64,
    }

    fn cf_string(text: &str) -> CFStringRef {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        unsafe {
            CFStringCreateWithCString(std::ptr::null(), bytes.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        }
    }

    pub fn list_studio_windows() -> Result<Vec<StudioWindow>> {
        unsafe {
            let list = CGWindowListCopyWindowInfo(
                K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
                0,
            );
            if list.is_null() {
                bail!("CGWindowListCopyWindowInfo failed");
            }
            let owner_name_key = cf_string("kCGWindowOwnerName");
            let owner_pid_key = cf_string("kCGWindowOwnerPID");
            let bounds_key = cf_string("kCGWindowBounds");
            let layer_key = cf_string("kCGWindowLayer");
            let number_key = cf_string("kCGWindowNumber");

            let mut windows = Vec::new();
            let count = CFArrayGetCount(list);
            for index in 0..count {
                let dict = CFArrayGetValueAtIndex(list, index) as CFDictionaryRef;
                if dict.is_null() {
                    continue;
                }
                let layer_value = CFDictionaryGetValue(dict, layer_key);
                let mut layer: i32 = -1;
                if !layer_value.is_null() {
                    CFNumberGetValue(
                        layer_value,
                        K_CF_NUMBER_INT_TYPE,
                        &mut layer as *mut i32 as *mut c_void,
                    );
                }
                if layer != 0 {
                    continue;
                }
                let name_value = CFDictionaryGetValue(dict, owner_name_key);
                if name_value.is_null() {
                    continue;
                }
                let mut name_buffer = [0u8; 256];
                if !CFStringGetCString(
                    name_value,
                    name_buffer.as_mut_ptr(),
                    name_buffer.len() as isize,
                    K_CF_STRING_ENCODING_UTF8,
                ) {
                    continue;
                }
                let name_len = name_buffer
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_buffer.len());
                let owner = String::from_utf8_lossy(&name_buffer[..name_len]).to_string();
                if !owner.contains("RobloxStudio") {
                    continue;
                }
                let pid_value = CFDictionaryGetValue(dict, owner_pid_key);
                if pid_value.is_null() {
                    continue;
                }
                let mut pid: i32 = 0;
                if !CFNumberGetValue(
                    pid_value,
                    K_CF_NUMBER_INT_TYPE,
                    &mut pid as *mut i32 as *mut c_void,
                ) {
                    continue;
                }
                let bounds_value = CFDictionaryGetValue(dict, bounds_key) as CFDictionaryRef;
                if bounds_value.is_null() {
                    continue;
                }
                let mut rect = CGRect {
                    origin: CGPoint { x: 0.0, y: 0.0 },
                    size: CGSize {
                        width: 0.0,
                        height: 0.0,
                    },
                };
                if !CGRectMakeWithDictionaryRepresentation(bounds_value, &mut rect) {
                    continue;
                }
                if rect.size.width < 320.0 || rect.size.height < 240.0 {
                    continue;
                }
                let number_value = CFDictionaryGetValue(dict, number_key);
                let mut window_number: i32 = 0;
                if !number_value.is_null() {
                    CFNumberGetValue(
                        number_value,
                        K_CF_NUMBER_INT_TYPE,
                        &mut window_number as *mut i32 as *mut c_void,
                    );
                }
                windows.push(StudioWindow {
                    label: format!("pid {pid}: {owner}"),
                    handle: WindowHandle {
                        pid,
                        window_number: window_number as u32,
                        origin_x: rect.origin.x,
                        origin_y: rect.origin.y,
                    },
                });
            }
            CFRelease(list);
            CFRelease(owner_name_key);
            CFRelease(owner_pid_key);
            CFRelease(bounds_key);
            CFRelease(layer_key);
            CFRelease(number_key);

            if windows.is_empty() {
                bail!("No visible Roblox Studio windows found");
            }
            windows.sort_by_key(|window| window.handle.pid);
            Ok(windows)
        }
    }

    fn post_mouse_event(handle: &WindowHandle, event_type: u32, button: u32, x: i32, y: i32) {
        let location = CGPoint {
            x: handle.origin_x + x as f64,
            y: handle.origin_y + y as f64,
        };
        unsafe {
            let event = CGEventCreateMouseEvent(std::ptr::null(), event_type, location, button);
            if !event.is_null() {
                CGEventPostToPid(handle.pid, event);
                CFRelease(event as CFTypeRef);
            }
        }
    }

    pub fn post_mouse_move(handle: &WindowHandle, x: i32, y: i32) -> Result<()> {
        post_mouse_event(handle, EVENT_MOUSE_MOVED, BUTTON_LEFT, x, y);
        Ok(())
    }

    pub fn post_probe_click(handle: &WindowHandle, x: i32, y: i32) -> Result<()> {
        const EVENT_OTHER_DOWN: u32 = 25;
        const EVENT_OTHER_UP: u32 = 26;
        const BUTTON_CENTER: u32 = 2;
        post_mouse_event(handle, EVENT_OTHER_DOWN, BUTTON_CENTER, x, y);
        std::thread::sleep(std::time::Duration::from_millis(20));
        post_mouse_event(handle, EVENT_OTHER_UP, BUTTON_CENTER, x, y);
        Ok(())
    }

    pub fn post_mouse_click(
        handle: &WindowHandle,
        x: i32,
        y: i32,
        right: bool,
        hold_ms: u64,
    ) -> Result<()> {
        let hold = std::time::Duration::from_millis(hold_ms.clamp(10, 2000));
        post_mouse_event(handle, EVENT_MOUSE_MOVED, BUTTON_LEFT, x, y);
        if right {
            post_mouse_event(handle, EVENT_RIGHT_DOWN, BUTTON_RIGHT, x, y);
            std::thread::sleep(hold);
            post_mouse_event(handle, EVENT_RIGHT_UP, BUTTON_RIGHT, x, y);
        } else {
            post_mouse_event(handle, EVENT_LEFT_DOWN, BUTTON_LEFT, x, y);
            std::thread::sleep(hold);
            post_mouse_event(handle, EVENT_LEFT_UP, BUTTON_LEFT, x, y);
        }
        Ok(())
    }

    pub fn post_key(handle: &WindowHandle, key: &super::KeySpec, hold_ms: u64) -> Result<()> {
        unsafe {
            let down = CGEventCreateKeyboardEvent(std::ptr::null(), key.mac_keycode, true);
            if down.is_null() {
                bail!(
                    "CGEventCreateKeyboardEvent failed; grant the Accessibility permission to \
                     the terminal running renium (System Settings > Privacy & Security)"
                );
            }
            CGEventPostToPid(handle.pid, down);
            CFRelease(down as CFTypeRef);
            std::thread::sleep(std::time::Duration::from_millis(hold_ms.clamp(10, 2000)));
            let up = CGEventCreateKeyboardEvent(std::ptr::null(), key.mac_keycode, false);
            if !up.is_null() {
                CGEventPostToPid(handle.pid, up);
                CFRelease(up as CFTypeRef);
            }
        }
        Ok(())
    }

    pub fn post_text(handle: &WindowHandle, text: &str) -> Result<()> {
        for character in text.chars() {
            let mut units = [0u16; 2];
            let encoded = character.encode_utf16(&mut units);
            unsafe {
                let down = CGEventCreateKeyboardEvent(std::ptr::null(), 0, true);
                if down.is_null() {
                    bail!(
                        "CGEventCreateKeyboardEvent failed; grant the Accessibility permission \
                         to the terminal running renium"
                    );
                }
                CGEventKeyboardSetUnicodeString(down, encoded.len(), encoded.as_ptr());
                CGEventPostToPid(handle.pid, down);
                CFRelease(down as CFTypeRef);
                let up = CGEventCreateKeyboardEvent(std::ptr::null(), 0, false);
                if !up.is_null() {
                    CGEventKeyboardSetUnicodeString(up, encoded.len(), encoded.as_ptr());
                    CGEventPostToPid(handle.pid, up);
                    CFRelease(up as CFTypeRef);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Ok(())
    }

    pub fn capture_window_png(
        handle: &WindowHandle,
        path: &std::path::Path,
    ) -> Result<(u32, u32)> {
        const K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW: u32 = 1 << 3;
        const K_CG_WINDOW_IMAGE_BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
        let null_rect = CGRect {
            origin: CGPoint {
                x: f64::INFINITY,
                y: f64::INFINITY,
            },
            size: CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        unsafe {
            let image = CGWindowListCreateImage(
                null_rect,
                K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW,
                handle.window_number,
                K_CG_WINDOW_IMAGE_BOUNDS_IGNORE_FRAMING,
            );
            if image.is_null() {
                bail!(
                    "CGWindowListCreateImage failed; grant the Screen Recording permission to \
                     the terminal running renium (System Settings > Privacy & Security)"
                );
            }
            let width = CGImageGetWidth(image) as u32;
            let height = CGImageGetHeight(image) as u32;
            let path_text = path.to_string_lossy();
            let path_bytes = path_text.as_bytes();
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                path_bytes.as_ptr(),
                path_bytes.len() as isize,
                false,
            );
            let png_type = cf_string("public.png");
            let destination =
                CGImageDestinationCreateWithURL(url, png_type, 1, std::ptr::null());
            let mut finalized = false;
            if !destination.is_null() {
                CGImageDestinationAddImage(destination, image, std::ptr::null());
                finalized = CGImageDestinationFinalize(destination);
                CFRelease(destination as CFTypeRef);
            }
            CFRelease(png_type);
            if !url.is_null() {
                CFRelease(url);
            }
            CGImageRelease(image);
            if !finalized {
                bail!("Failed to write PNG to {}", path.display());
            }
            Ok((width, height))
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::StudioWindow;
    use anyhow::{bail, Result};

    #[derive(Clone)]
    pub struct WindowHandle;

    pub fn list_studio_windows() -> Result<Vec<StudioWindow>> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_mouse_move(_handle: &WindowHandle, _x: i32, _y: i32) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_probe_click(_handle: &WindowHandle, _x: i32, _y: i32) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_text(_handle: &WindowHandle, _text: &str) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn capture_window_png(
        _handle: &WindowHandle,
        _path: &std::path::Path,
    ) -> Result<(u32, u32)> {
        bail!("Window capture is only supported on Windows and macOS")
    }

    pub fn post_mouse_click(
        _handle: &WindowHandle,
        _x: i32,
        _y: i32,
        _right: bool,
        _hold_ms: u64,
    ) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_key(_handle: &WindowHandle, _key: &super::KeySpec, _hold_ms: u64) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }
}
