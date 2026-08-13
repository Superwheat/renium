use anyhow::{Result, bail};

pub struct StudioWindow {
    pub label: String,
    handle: platform::WindowHandle,
    #[cfg(windows)]
    _reminimize: Option<platform::ReminimizeGuard>,
}

#[cfg(any(windows, target_os = "macos"))]
pub fn window_for_pid(pid: u32, viewport: Option<(i32, i32)>) -> Result<StudioWindow> {
    platform::window_for_pid(pid, viewport, true)
}

#[cfg(any(windows, target_os = "macos"))]
pub fn recording_window_for_pid(pid: u32, viewport: Option<(i32, i32)>) -> Result<StudioWindow> {
    platform::window_for_pid(pid, viewport, false)
}

#[cfg(windows)]
pub fn verified_studio_window_for_pid<F>(pid: u32, mut set_probe_phase: F) -> Result<StudioWindow>
where
    F: FnMut(u8, &[u32]) -> Result<()>,
{
    platform::verified_studio_window_for_pid(pid, &mut set_probe_phase, true)
}

#[cfg(windows)]
pub fn verified_recording_window_for_pid<F>(
    pid: u32,
    mut set_probe_phase: F,
) -> Result<StudioWindow>
where
    F: FnMut(u8, &[u32]) -> Result<()>,
{
    platform::verified_studio_window_for_pid(pid, &mut set_probe_phase, false)
}

#[cfg(windows)]
pub fn process_executable_path(pid: u32) -> Result<std::path::PathBuf> {
    platform::process_executable_path(pid)
}

#[cfg(windows)]
pub fn studio_window_title(pid: u32) -> Result<String> {
    platform::studio_window_title(pid)
}

#[cfg(windows)]
pub fn terminate_studio_process(pid: u32) -> Result<()> {
    platform::terminate_studio_process(pid)
}

#[cfg(target_os = "macos")]
pub fn terminate_studio_process(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid).map_err(|_| anyhow::anyhow!("Studio PID is out of range"))?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn frontmost_studio_pid() -> Option<u32> {
    platform::frontmost_studio_pid()
}

pub fn post_mouse_move(window: &StudioWindow, x: i32, y: i32) -> Result<()> {
    platform::post_mouse_move(&window.handle, x, y)
}

pub fn post_mouse_button(
    window: &StudioWindow,
    x: i32,
    y: i32,
    right: bool,
    down: bool,
) -> Result<()> {
    platform::post_mouse_button(&window.handle, x, y, right, down)
}

pub fn post_mouse_scroll(window: &StudioWindow, x: i32, y: i32, delta: i32) -> Result<()> {
    platform::post_mouse_scroll(&window.handle, x, y, delta)
}

pub fn post_text(window: &StudioWindow, text: &str) -> Result<()> {
    platform::post_text(&window.handle, text)
}

pub fn capture_window_png(window: &StudioWindow, path: &std::path::Path) -> Result<(u32, u32)> {
    platform::capture_window_png(&window.handle, path)
}

pub fn capture_window_rgba(window: &StudioWindow) -> Result<(u32, u32, Vec<u8>)> {
    platform::capture_window_rgba(&window.handle)
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

pub fn post_key_state(window: &StudioWindow, key: &KeySpec, down: bool) -> Result<()> {
    platform::post_key_state(&window.handle, key, down)
}

pub struct KeySpec {
    #[cfg(any(windows, target_os = "macos"))]
    platform_code: u16,
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
            "DELETE" => Some((0x2E, 117, "Delete")),
            "INSERT" => Some((0x2D, 114, "Insert")),
            "HOME" => Some((0x24, 115, "Home")),
            "END" => Some((0x23, 119, "End")),
            "PAGEUP" => Some((0x21, 116, "PageUp")),
            "PAGEDOWN" => Some((0x22, 121, "PageDown")),
            "UP" => Some((0x26, 126, "Up")),
            "DOWN" => Some((0x28, 125, "Down")),
            "LEFT" => Some((0x25, 123, "Left")),
            "RIGHT" => Some((0x27, 124, "Right")),
            "SHIFT" | "LEFTSHIFT" => Some((0xA0, 56, "LeftShift")),
            "RIGHTSHIFT" => Some((0xA1, 60, "RightShift")),
            "CTRL" | "CONTROL" | "LEFTCONTROL" => Some((0xA2, 59, "LeftControl")),
            "RIGHTCONTROL" => Some((0xA3, 62, "RightControl")),
            "ALT" | "LEFTALT" => Some((0xA4, 58, "LeftAlt")),
            "RIGHTALT" => Some((0xA5, 61, "RightAlt")),
            "LEFTMETA" | "LEFTSUPER" => Some((0x5B, 55, "LeftMeta")),
            "RIGHTMETA" | "RIGHTSUPER" => Some((0x5C, 54, "RightMeta")),
            "CAPSLOCK" => Some((0x14, 57, "CapsLock")),
            "NUMLOCK" => Some((0x90, 71, "NumLock")),
            "SCROLLLOCK" => Some((0x91, 107, "ScrollLock")),
            "PAUSE" | "BREAK" => Some((0x13, 113, "Pause")),
            "PRINT" => Some((0x2C, 105, "Print")),
            "CLEAR" => Some((0x0C, 71, "Clear")),
            "ZERO" => Some((b'0' as u16, 29, "0")),
            "ONE" => Some((b'1' as u16, 18, "1")),
            "TWO" => Some((b'2' as u16, 19, "2")),
            "THREE" => Some((b'3' as u16, 20, "3")),
            "FOUR" => Some((b'4' as u16, 21, "4")),
            "FIVE" => Some((b'5' as u16, 23, "5")),
            "SIX" => Some((b'6' as u16, 22, "6")),
            "SEVEN" => Some((b'7' as u16, 26, "7")),
            "EIGHT" => Some((b'8' as u16, 28, "8")),
            "NINE" => Some((b'9' as u16, 25, "9")),
            "MINUS" => Some((0xBD, 27, "Minus")),
            "EQUALS" => Some((0xBB, 24, "Equals")),
            "LEFTBRACKET" => Some((0xDB, 33, "LeftBracket")),
            "RIGHTBRACKET" => Some((0xDD, 30, "RightBracket")),
            "BACKSLASH" => Some((0xDC, 42, "BackSlash")),
            "SEMICOLON" => Some((0xBA, 41, "Semicolon")),
            "QUOTE" => Some((0xDE, 39, "Quote")),
            "COMMA" => Some((0xBC, 43, "Comma")),
            "PERIOD" => Some((0xBE, 47, "Period")),
            "SLASH" => Some((0xBF, 44, "Slash")),
            "BACKQUOTE" => Some((0xC0, 50, "Backquote")),
            "F1" => Some((0x70, 122, "F1")),
            "F2" => Some((0x71, 120, "F2")),
            "F3" => Some((0x72, 99, "F3")),
            "F4" => Some((0x73, 118, "F4")),
            "F5" => Some((0x74, 96, "F5")),
            "F6" => Some((0x75, 97, "F6")),
            "F7" => Some((0x76, 98, "F7")),
            "F8" => Some((0x77, 100, "F8")),
            "F9" => Some((0x78, 101, "F9")),
            "F10" => Some((0x79, 109, "F10")),
            "F11" => Some((0x7A, 103, "F11")),
            "F12" => Some((0x7B, 111, "F12")),
            "F13" => Some((0x7C, 105, "F13")),
            "F14" => Some((0x7D, 107, "F14")),
            "F15" => Some((0x7E, 113, "F15")),
            _ => None,
        }
    };
    match entry {
        Some((_windows_vk, _mac_keycode, canonical)) => Ok(KeySpec {
            #[cfg(windows)]
            platform_code: _windows_vk,
            #[cfg(target_os = "macos")]
            platform_code: _mac_keycode,
            name: canonical,
        }),
        None => bail!("Unsupported keyboard key '{trimmed}'"),
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

#[cfg(windows)]
fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    use anyhow::Context;
    let file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_filter(png::FilterType::Paeth);
    let _ = encoder.add_text_chunk(
        "Software".to_string(),
        "capture-rs 0.5.3+build.57".to_string(),
    );
    let mut writer = encoder
        .write_header()
        .context("Failed to write PNG header")?;
    writer
        .write_image_data(rgba)
        .context("Failed to write PNG data")?;
    Ok(())
}

#[cfg(windows)]
mod platform {
    use super::StudioWindow;
    use anyhow::{Context, Result, bail};

    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GetClassNameW, GetClientRect, GetWindowTextW,
        GetWindowThreadProcessId, IsChild, IsIconic, IsWindowVisible, PostMessageW,
        SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, ShowWindow, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    };

    const MK_LBUTTON: WPARAM = 0x0001;
    const MK_RBUTTON: WPARAM = 0x0002;

    pub struct WindowHandle {
        viewport: isize,
        capture: isize,
        render_matched: bool,
        capture_verified: bool,
        verified_frame: Option<(u32, u32, Vec<u8>)>,
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

    fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        (unsafe { GetClientRect(hwnd, &mut rect) } != 0)
            .then_some((rect.right - rect.left, rect.bottom - rect.top))
    }

    fn visible_client_size(hwnd: HWND) -> Option<(i32, i32)> {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            None
        } else {
            client_size(hwnd)
        }
    }

    unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut EnumChildState) };
        let Some((width, height)) = visible_client_size(hwnd) else {
            return 1;
        };
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
        let click_target = if state.exact != 0 {
            state.exact
        } else {
            state.best
        };
        let capture = if state.render != 0 {
            state.render
        } else {
            click_target
        };
        WindowHandle {
            viewport: click_target,
            capture,
            render_matched: state.render != 0,
            capture_verified: false,
            verified_frame: None,
            offset_x: 0,
            offset_y: 0,
        }
    }

    #[derive(Clone, Copy)]
    struct CaptureCandidate {
        hwnd: isize,
        width: i32,
        height: i32,
    }

    struct EnumCaptureState {
        candidates: Vec<CaptureCandidate>,
    }

    unsafe extern "system" fn enum_capture_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut EnumCaptureState) };
        let Some((width, height)) = visible_client_size(hwnd) else {
            return 1;
        };
        if width >= 96 && height >= 96 {
            state.candidates.push(CaptureCandidate {
                hwnd: hwnd as isize,
                width,
                height,
            });
        }
        1
    }

    fn capture_candidates(top: isize) -> Vec<CaptureCandidate> {
        let mut state = EnumCaptureState {
            candidates: Vec::new(),
        };
        let _dpi = ThreadDpiAwareness::per_monitor_v2();
        if let Some((width, height)) = client_size(top as HWND)
            && width >= 96
            && height >= 96
        {
            state.candidates.push(CaptureCandidate {
                hwnd: top,
                width,
                height,
            });
        }
        unsafe {
            EnumChildWindows(
                top as HWND,
                Some(enum_capture_proc),
                &mut state as *mut EnumCaptureState as LPARAM,
            );
        }
        state.candidates
    }

    fn capture_hwnd_pixels(hwnd: isize, allow_fallback: bool) -> Result<(u32, u32, Vec<u8>)> {
        use windows_sys::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SRCCOPY, SelectObject,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
        #[link(name = "user32")]
        unsafe extern "system" {
            fn PrintWindow(hwnd: HWND, hdc: *mut std::ffi::c_void, flags: u32) -> i32;
        }
        const PW_CLIENTONLY: u32 = 0x1;
        const PW_RENDERFULLCONTENT: u32 = 0x2;

        let _dpi = ThreadDpiAwareness::per_monitor_v2();
        let hwnd = hwnd as HWND;
        if unsafe { IsWindow(hwnd) } == 0 {
            bail!("Capture target no longer exists");
        }
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
            if memory_dc.is_null() {
                ReleaseDC(hwnd, window_dc);
                bail!("CreateCompatibleDC failed for capture target");
            }
            let bitmap = CreateCompatibleBitmap(window_dc, width, height);
            if bitmap.is_null() {
                DeleteDC(memory_dc);
                ReleaseDC(hwnd, window_dc);
                bail!("CreateCompatibleBitmap failed for capture target");
            }
            let previous = SelectObject(memory_dc, bitmap as _);
            let printed = PrintWindow(hwnd, memory_dc, PW_CLIENTONLY | PW_RENDERFULLCONTENT);
            if printed == 0 && !allow_fallback {
                SelectObject(memory_dc, previous);
                DeleteObject(bitmap as _);
                DeleteDC(memory_dc);
                ReleaseDC(hwnd, window_dc);
                bail!("PrintWindow rejected the verified capture target");
            }
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
            Ok((width as u32, height as u32, pixels))
        }
    }

    struct ProbeFrame {
        candidate: CaptureCandidate,
        pixels: Vec<u8>,
    }

    fn capture_probe_frames(candidates: &[CaptureCandidate]) -> Vec<ProbeFrame> {
        candidates
            .iter()
            .filter_map(|candidate| {
                let (width, height, pixels) = capture_hwnd_pixels(candidate.hwnd, false).ok()?;
                if width as i32 != candidate.width || height as i32 != candidate.height {
                    return None;
                }
                Some(ProbeFrame {
                    candidate: *candidate,
                    pixels,
                })
            })
            .collect()
    }

    fn probe_palette(mut state: u64) -> [u32; 16] {
        let mut colors = [0u32; 16];
        for color in &mut colors {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let red = if state & 1 == 0 { 16 } else { 240 };
            let green = if state & 2 == 0 { 16 } else { 240 };
            let blue = if state & 4 == 0 { 16 } else { 240 };
            *color = ((red as u32) << 16) | ((green as u32) << 8) | blue as u32;
        }
        colors
    }

    fn probe_transition_matches(
        before: &ProbeFrame,
        after: &ProbeFrame,
        first: &[u32; 16],
        second: &[u32; 16],
    ) -> bool {
        let width = before.candidate.width as usize;
        let height = before.candidate.height as usize;
        if before.pixels.len() != width.saturating_mul(height).saturating_mul(4)
            || after.pixels.len() != before.pixels.len()
        {
            return false;
        }
        first
            .iter()
            .zip(second)
            .enumerate()
            .all(|(index, (first_color, second_color))| {
                let column = index % 4;
                let row = index / 4;
                let center_x = ((column * 2 + 1) * width) / 8;
                let center_y = ((row * 2 + 1) * height) / 8;
                let first_rgb = [
                    ((first_color >> 16) & 0xff) as i16,
                    ((first_color >> 8) & 0xff) as i16,
                    (first_color & 0xff) as i16,
                ];
                let second_rgb = [
                    ((second_color >> 16) & 0xff) as i16,
                    ((second_color >> 8) & 0xff) as i16,
                    (second_color & 0xff) as i16,
                ];
                let mut matching = 0usize;
                for dy in -2isize..=2 {
                    for dx in -2isize..=2 {
                        let x = center_x.saturating_add_signed(dx).min(width - 1);
                        let y = center_y.saturating_add_signed(dy).min(height - 1);
                        let offset = (y * width + x) * 4;
                        let channels_match = (0..3).all(|channel| {
                            let expected = second_rgb[channel] - first_rgb[channel];
                            let actual = after.pixels[offset + channel] as i16
                                - before.pixels[offset + channel] as i16;
                            actual.signum() == expected.signum() && (2..=8).contains(&actual.abs())
                        });
                        if channels_match {
                            matching += 1;
                        }
                    }
                }
                matching >= 20
            })
    }

    fn probe_frames_equivalent(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let different = a
            .chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(x, y)| {
                x[0].abs_diff(y[0])
                    .max(x[1].abs_diff(y[1]))
                    .max(x[2].abs_diff(y[2]))
                    > 2
            })
            .count();
        different * 10_000 <= a.len() / 4
    }

    fn windows_for_pid(pid: u32) -> Vec<(isize, u32, String)> {
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
        state.windows
    }

    fn window_class(hwnd: isize) -> String {
        let mut class = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd as HWND, class.as_mut_ptr(), class.len() as i32) };
        if len <= 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&class[..len as usize])
        }
    }

    pub fn process_executable_path(pid: u32) -> Result<std::path::PathBuf> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            bail!("Could not open Studio process {pid}");
        }
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
        unsafe { CloseHandle(handle) };
        if ok == 0 || length == 0 {
            bail!("Could not read the executable path for Studio process {pid}");
        }
        Ok(std::path::PathBuf::from(String::from_utf16_lossy(
            &buffer[..length as usize],
        )))
    }

    pub fn studio_window_title(pid: u32) -> Result<String> {
        let windows = windows_for_pid(pid);
        windows
            .iter()
            .find(|(hwnd, _, title)| {
                window_class(*hwnd).starts_with("Qt") && title.ends_with(" - Roblox Studio")
            })
            .or_else(|| {
                windows
                    .iter()
                    .find(|(hwnd, _, _)| window_class(*hwnd).starts_with("Qt"))
            })
            .map(|(_, _, title)| title.clone())
            .with_context(|| format!("No Studio window found for process {pid}"))
    }

    pub fn terminate_studio_process(pid: u32) -> Result<()> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Ok(());
        }
        if unsafe { TerminateProcess(handle, 0) } == 0 {
            unsafe { CloseHandle(handle) };
            bail!("Could not terminate Studio process {pid}");
        }
        if unsafe { WaitForSingleObject(handle, 5000) } != 0 {
            unsafe { CloseHandle(handle) };
            bail!("Studio process {pid} did not terminate");
        }
        unsafe { CloseHandle(handle) };
        Ok(())
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

    pub fn window_for_pid(
        pid: u32,
        viewport: Option<(i32, i32)>,
        restore_minimized: bool,
    ) -> Result<StudioWindow> {
        let (hwnd, pid, title) = windows_for_pid(pid)
            .into_iter()
            .max_by_key(|(hwnd, _, _)| {
                client_size(*hwnd as HWND)
                    .map(|(width, height)| width as i64 * height as i64)
                    .unwrap_or_default()
            })
            .with_context(|| format!("No visible window found for Studio process {pid}"))?;
        let reminimize = if unsafe { IsIconic(hwnd as HWND) } != 0 {
            if !restore_minimized {
                bail!("The selected Studio window is minimized; restore it before recording");
            }
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

    pub fn verified_studio_window_for_pid<F>(
        pid: u32,
        set_probe_phase: &mut F,
        restore_minimized: bool,
    ) -> Result<StudioWindow>
    where
        F: FnMut(u8, &[u32]) -> Result<()>,
    {
        let (hwnd, pid, title) = windows_for_pid(pid)
            .into_iter()
            .find(|(hwnd, _, title)| {
                window_class(*hwnd).starts_with("Qt") && title.ends_with(" - Roblox Studio")
            })
            .with_context(|| format!("No visible Studio window found for process {pid}"))?;
        let reminimize = if unsafe { IsIconic(hwnd as HWND) } != 0 {
            if !restore_minimized {
                bail!("The selected Studio window is minimized; restore it before recording");
            }
            unsafe { ShowWindow(hwnd as HWND, SW_SHOWNOACTIVATE) };
            std::thread::sleep(std::time::Duration::from_millis(400));
            Some(ReminimizeGuard { top: hwnd })
        } else {
            None
        };
        let mut probe_started = false;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ ((pid as u64) << 32)
            ^ hwnd as u64;
        let first_colors = probe_palette(seed ^ 0x4f1b_7ca3_8d25_e691);
        let second_colors = first_colors.map(|color| 0x00ff_ffff ^ color);
        let selection = (|| {
            set_probe_phase(0, &first_colors)?;
            probe_started = true;
            std::thread::sleep(std::time::Duration::from_millis(60));
            let candidates = capture_candidates(hwnd);
            if candidates.is_empty() {
                bail!("Studio exposed no capturable windows for viewport verification");
            }
            let first = capture_probe_frames(&candidates);
            set_probe_phase(1, &second_colors)?;
            std::thread::sleep(std::time::Duration::from_millis(60));
            let second = capture_probe_frames(&candidates);

            let mut contenders = Vec::new();
            for after in second {
                let Some(before) = first.iter().find(|entry| {
                    entry.candidate.hwnd == after.candidate.hwnd
                        && entry.candidate.width == after.candidate.width
                        && entry.candidate.height == after.candidate.height
                }) else {
                    continue;
                };
                if probe_transition_matches(before, &after, &first_colors, &second_colors) {
                    contenders.push(after);
                }
            }
            if contenders.is_empty() {
                bail!(
                    "Could not prove which Studio child is the rendered viewport; no window reproduced both capture identity patterns"
                );
            }
            Ok(contenders)
        })();
        let stop_result = if probe_started {
            set_probe_phase(2, &[])
        } else {
            Ok(())
        };
        std::thread::sleep(std::time::Duration::from_millis(80));
        let contenders = selection?;
        stop_result.context("Failed to remove the Studio viewport capture probe")?;
        let baseline = capture_probe_frames(
            &contenders
                .iter()
                .map(|frame| frame.candidate)
                .collect::<Vec<_>>(),
        );
        let composite_hosts = baseline
            .iter()
            .filter(|host| {
                baseline.iter().any(|child| {
                    child.candidate.hwnd != host.candidate.hwnd
                        && unsafe {
                            IsChild(host.candidate.hwnd as HWND, child.candidate.hwnd as HWND)
                        } != 0
                        && probe_frames_equivalent(&host.pixels, &child.pixels)
                })
            })
            .collect::<Vec<_>>();
        let leaves = composite_hosts
            .iter()
            .filter(|host| {
                !composite_hosts.iter().any(|other| {
                    other.candidate.hwnd != host.candidate.hwnd
                        && unsafe {
                            IsChild(host.candidate.hwnd as HWND, other.candidate.hwnd as HWND)
                        } != 0
                })
            })
            .collect::<Vec<_>>();
        if leaves.len() != 1 || leaves[0].candidate.hwnd == hwnd {
            bail!(
                "Studio exposed no unique probe-free composited viewport host; refusing to guess"
            );
        }
        let root = leaves[0];
        let candidate = root.candidate;
        let handle = WindowHandle {
            viewport: candidate.hwnd,
            capture: candidate.hwnd,
            render_matched: true,
            capture_verified: true,
            verified_frame: Some((
                candidate.width as u32,
                candidate.height as u32,
                root.pixels.clone(),
            )),
            offset_x: 0,
            offset_y: 0,
        };
        Ok(StudioWindow {
            label: format!("verified viewport for pid {pid}: {title}"),
            handle,
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

    pub fn post_mouse_move(handle: &WindowHandle, x: i32, y: i32) -> Result<()> {
        post(handle.viewport, WM_MOUSEMOVE, 0, mouse_lparam(handle, x, y))
    }

    pub fn post_mouse_button(
        handle: &WindowHandle,
        x: i32,
        y: i32,
        right: bool,
        down: bool,
    ) -> Result<()> {
        let (message, state) = match (right, down) {
            (false, true) => (WM_LBUTTONDOWN, MK_LBUTTON),
            (false, false) => (WM_LBUTTONUP, 0),
            (true, true) => (WM_RBUTTONDOWN, MK_RBUTTON),
            (true, false) => (WM_RBUTTONUP, 0),
        };
        post(handle.viewport, message, state, mouse_lparam(handle, x, y))
    }

    pub fn post_mouse_scroll(handle: &WindowHandle, x: i32, y: i32, delta: i32) -> Result<()> {
        let mut point = POINT {
            x: x + handle.offset_x,
            y: y + handle.offset_y,
        };
        if unsafe { ClientToScreen(handle.viewport as HWND, &raw mut point) } == 0 {
            bail!("ClientToScreen failed for the target Studio window");
        }
        let position = (((point.y as u32) << 16) | (point.x as u32 & 0xFFFF)) as i32 as LPARAM;
        let wheel_delta = delta.clamp(-10, 10) * 120;
        let wheel = ((wheel_delta as u16 as usize) << 16) as WPARAM;
        post(handle.viewport, WM_MOUSEWHEEL, wheel, position)
    }

    pub fn post_mouse_click(
        handle: &WindowHandle,
        x: i32,
        y: i32,
        right: bool,
        hold_ms: u64,
    ) -> Result<()> {
        let hold = std::time::Duration::from_millis(hold_ms.clamp(10, 2000));
        post_mouse_move(handle, x, y)?;
        post_mouse_button(handle, x, y, right, true)?;
        std::thread::sleep(hold);
        post_mouse_button(handle, x, y, right, false)
    }

    pub fn post_key_state(handle: &WindowHandle, key: &super::KeySpec, down: bool) -> Result<()> {
        let vk = key.platform_code as usize;
        let scan = unsafe { MapVirtualKeyW(key.platform_code as u32, MAPVK_VK_TO_VSC) } as usize;
        let state = if down {
            1usize
        } else {
            1usize | (1 << 30) | (1 << 31)
        };
        let message = if down { WM_KEYDOWN } else { WM_KEYUP };
        post(
            handle.viewport,
            message,
            vk as WPARAM,
            (state | (scan << 16)) as isize as LPARAM,
        )
    }

    pub fn post_key(handle: &WindowHandle, key: &super::KeySpec, hold_ms: u64) -> Result<()> {
        post_key_state(handle, key, true)?;
        std::thread::sleep(std::time::Duration::from_millis(hold_ms.clamp(10, 2000)));
        post_key_state(handle, key, false)
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
        if let Some((width, height, pixels)) = &handle.verified_frame {
            super::write_png(path, *width, *height, pixels)?;
            return Ok((*width, *height));
        }
        let (width, height, pixels) =
            capture_hwnd_pixels(handle.capture, !handle.capture_verified)?;
        super::write_png(path, width, height, &pixels)?;
        Ok((width, height))
    }

    pub fn capture_window_rgba(handle: &WindowHandle) -> Result<(u32, u32, Vec<u8>)> {
        capture_hwnd_pixels(handle.capture, !handle.capture_verified)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, bail};
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CGEventRef = *mut c_void;

    #[repr(C)]

    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]

    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]

    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
    const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    const K_CF_NUMBER_INT_TYPE: isize = 9;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
    const K_CG_BITMAP_BYTE_ORDER_32_BIG: u32 = 4 << 12;

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
        fn CGEventCreateScrollWheelEvent(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            ...
        ) -> CGEventRef;
        fn CGEventPostToPid(pid: i32, event: CGEventRef);
        fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
        fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, string: *const u16);
        fn CGWindowListCreateImage(
            screen_bounds: CGRect,
            options: u32,
            window_id: u32,
            image_options: u32,
        ) -> *mut c_void;
        fn CGImageGetWidth(image: *const c_void) -> usize;
        fn CGImageGetHeight(image: *const c_void) -> usize;
        fn CGImageRelease(image: *mut c_void);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            bitmap_info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *const c_void);
        fn CGContextRelease(context: *mut c_void);
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

    struct WindowRecord {
        owner: String,
        pid: i32,
        window_number: u32,
        rect: CGRect,
    }

    fn cf_number_i32(value: CFTypeRef) -> Option<i32> {
        if value.is_null() {
            return None;
        }
        let mut number = 0;
        unsafe {
            CFNumberGetValue(
                value,
                K_CF_NUMBER_INT_TYPE,
                (&raw mut number).cast::<c_void>(),
            )
            .then_some(number)
        }
    }

    fn cf_string_value(value: CFTypeRef) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let mut bytes = [0u8; 256];
        unsafe {
            if !CFStringGetCString(
                value,
                bytes.as_mut_ptr(),
                bytes.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
            ) {
                return None;
            }
        }
        let length = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        Some(String::from_utf8_lossy(&bytes[..length]).into_owned())
    }

    fn cf_rect(value: CFTypeRef) -> Option<CGRect> {
        if value.is_null() {
            return None;
        }
        let mut rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        unsafe {
            CGRectMakeWithDictionaryRepresentation(value.cast(), &raw mut rect).then_some(rect)
        }
    }

    fn is_studio_owner(owner: &str) -> bool {
        owner.contains("RobloxStudio") || owner.contains("Roblox Studio")
    }

    fn studio_window_records(on_screen_only: bool) -> Result<Vec<WindowRecord>> {
        unsafe {
            let options = K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS
                | if on_screen_only {
                    K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY
                } else {
                    0
                };
            let list = CGWindowListCopyWindowInfo(options, 0);
            if list.is_null() {
                bail!("CGWindowListCopyWindowInfo failed");
            }
            let owner_name_key = cf_string("kCGWindowOwnerName");
            let owner_pid_key = cf_string("kCGWindowOwnerPID");
            let bounds_key = cf_string("kCGWindowBounds");
            let layer_key = cf_string("kCGWindowLayer");
            let number_key = cf_string("kCGWindowNumber");
            let mut records = Vec::new();
            for index in 0..CFArrayGetCount(list) {
                let dict = CFArrayGetValueAtIndex(list, index) as CFDictionaryRef;
                if dict.is_null() || cf_number_i32(CFDictionaryGetValue(dict, layer_key)) != Some(0)
                {
                    continue;
                }
                let Some(owner) = cf_string_value(CFDictionaryGetValue(dict, owner_name_key))
                else {
                    continue;
                };
                let Some(pid) = cf_number_i32(CFDictionaryGetValue(dict, owner_pid_key)) else {
                    continue;
                };
                let Some(rect) = cf_rect(CFDictionaryGetValue(dict, bounds_key)) else {
                    continue;
                };
                let window_number = cf_number_i32(CFDictionaryGetValue(dict, number_key))
                    .and_then(|number| u32::try_from(number).ok())
                    .unwrap_or(0);
                records.push(WindowRecord {
                    owner,
                    pid,
                    window_number,
                    rect,
                });
            }
            CFRelease(list);
            CFRelease(owner_name_key);
            CFRelease(owner_pid_key);
            CFRelease(bounds_key);
            CFRelease(layer_key);
            CFRelease(number_key);
            Ok(records)
        }
    }

    pub fn frontmost_studio_pid() -> Option<u32> {
        studio_window_records(true)
            .ok()?
            .into_iter()
            .find_map(|record| {
                if record.pid > 0
                    && record.rect.size.width >= 160.0
                    && record.rect.size.height >= 120.0
                    && is_studio_owner(&record.owner)
                {
                    u32::try_from(record.pid).ok()
                } else {
                    None
                }
            })
    }

    pub fn window_for_pid(
        pid: u32,
        viewport: Option<(i32, i32)>,
        _restore_minimized: bool,
    ) -> Result<StudioWindow> {
        let pid = i32::try_from(pid).map_err(|_| anyhow::anyhow!("Studio PID is out of range"))?;
        let mut records = studio_window_records(false)?
            .into_iter()
            .filter(|record| {
                record.pid == pid
                    && record.rect.size.width >= 320.0
                    && record.rect.size.height >= 240.0
            })
            .collect::<Vec<_>>();
        if records.is_empty() {
            bail!("No visible Roblox Studio window belongs to PID {pid}");
        }
        records.sort_by(|left, right| {
            let score = |record: &WindowRecord| match viewport {
                Some((width, height)) => {
                    (record.rect.size.width - width as f64).abs()
                        + (record.rect.size.height - height as f64).abs()
                }
                None => -(record.rect.size.width * record.rect.size.height),
            };
            score(left).total_cmp(&score(right))
        });
        let record = records.remove(0);
        Ok(StudioWindow {
            label: format!("pid {}: {}", record.pid, record.owner),
            handle: WindowHandle {
                pid: record.pid,
                window_number: record.window_number,
                origin_x: record.rect.origin.x,
                origin_y: record.rect.origin.y,
            },
        })
    }

    fn post_mouse_event(
        handle: &WindowHandle,
        event_type: u32,
        button: u32,
        x: i32,
        y: i32,
    ) -> Result<()> {
        let location = CGPoint {
            x: handle.origin_x + x as f64,
            y: handle.origin_y + y as f64,
        };
        unsafe {
            let event = CGEventCreateMouseEvent(std::ptr::null(), event_type, location, button);
            if event.is_null() {
                bail!(
                    "CGEventCreateMouseEvent failed; grant the Accessibility permission to the terminal running renium"
                );
            }
            CGEventPostToPid(handle.pid, event);
            CFRelease(event as CFTypeRef);
        }
        Ok(())
    }

    pub fn post_mouse_move(handle: &WindowHandle, x: i32, y: i32) -> Result<()> {
        post_mouse_event(handle, EVENT_MOUSE_MOVED, BUTTON_LEFT, x, y)
    }

    pub fn post_mouse_button(
        handle: &WindowHandle,
        x: i32,
        y: i32,
        right: bool,
        down: bool,
    ) -> Result<()> {
        let (event_type, button) = match (right, down) {
            (false, true) => (EVENT_LEFT_DOWN, BUTTON_LEFT),
            (false, false) => (EVENT_LEFT_UP, BUTTON_LEFT),
            (true, true) => (EVENT_RIGHT_DOWN, BUTTON_RIGHT),
            (true, false) => (EVENT_RIGHT_UP, BUTTON_RIGHT),
        };
        post_mouse_event(handle, event_type, button, x, y)
    }

    pub fn post_mouse_scroll(handle: &WindowHandle, x: i32, y: i32, delta: i32) -> Result<()> {
        const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
        unsafe {
            let event = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_LINE,
                1,
                delta.clamp(-10, 10),
            );
            if event.is_null() {
                bail!("CGEventCreateScrollWheelEvent failed");
            }
            CGEventSetLocation(
                event,
                CGPoint {
                    x: handle.origin_x + x as f64,
                    y: handle.origin_y + y as f64,
                },
            );
            CGEventPostToPid(handle.pid, event);
            CFRelease(event as CFTypeRef);
        }
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
        post_mouse_move(handle, x, y)?;
        post_mouse_button(handle, x, y, right, true)?;
        std::thread::sleep(hold);
        post_mouse_button(handle, x, y, right, false)?;
        Ok(())
    }

    pub fn post_key_state(handle: &WindowHandle, key: &super::KeySpec, down: bool) -> Result<()> {
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), key.platform_code, down);
            if event.is_null() {
                bail!(
                    "CGEventCreateKeyboardEvent failed; grant the Accessibility permission to \
                     the terminal running renium (System Settings > Privacy & Security)"
                );
            }
            CGEventPostToPid(handle.pid, event);
            CFRelease(event as CFTypeRef);
        }
        Ok(())
    }

    pub fn post_key(handle: &WindowHandle, key: &super::KeySpec, hold_ms: u64) -> Result<()> {
        post_key_state(handle, key, true)?;
        std::thread::sleep(std::time::Duration::from_millis(hold_ms.clamp(10, 2000)));
        post_key_state(handle, key, false)
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

    pub fn capture_window_png(handle: &WindowHandle, path: &std::path::Path) -> Result<(u32, u32)> {
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
            let destination = CGImageDestinationCreateWithURL(url, png_type, 1, std::ptr::null());
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

    pub fn capture_window_rgba(handle: &WindowHandle) -> Result<(u32, u32, Vec<u8>)> {
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
                    "CGWindowListCreateImage failed; grant the Screen Recording permission to the terminal running renium (System Settings > Privacy & Security)"
                );
            }
            let width = CGImageGetWidth(image);
            let height = CGImageGetHeight(image);
            let mut pixels = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
            let color_space = CGColorSpaceCreateDeviceRGB();
            let context = CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                width * 4,
                color_space,
                K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST | K_CG_BITMAP_BYTE_ORDER_32_BIG,
            );
            if !context.is_null() {
                CGContextDrawImage(
                    context,
                    CGRect {
                        origin: CGPoint { x: 0.0, y: 0.0 },
                        size: CGSize {
                            width: width as f64,
                            height: height as f64,
                        },
                    },
                    image,
                );
                CGContextRelease(context);
            }
            if !color_space.is_null() {
                CGColorSpaceRelease(color_space);
            }
            CGImageRelease(image);
            if context.is_null() {
                bail!("CGBitmapContextCreate failed for Studio recording");
            }
            Ok((width as u32, height as u32, pixels))
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use anyhow::{Result, bail};

    pub struct WindowHandle;

    pub fn post_mouse_move(_handle: &WindowHandle, _x: i32, _y: i32) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_mouse_button(
        _handle: &WindowHandle,
        _x: i32,
        _y: i32,
        _right: bool,
        _down: bool,
    ) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }

    pub fn post_mouse_scroll(_handle: &WindowHandle, _x: i32, _y: i32, _delta: i32) -> Result<()> {
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

    pub fn capture_window_rgba(_handle: &WindowHandle) -> Result<(u32, u32, Vec<u8>)> {
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

    pub fn post_key_state(
        _handle: &WindowHandle,
        _key: &super::KeySpec,
        _down: bool,
    ) -> Result<()> {
        bail!("Input injection is only supported on Windows and macOS")
    }
}
