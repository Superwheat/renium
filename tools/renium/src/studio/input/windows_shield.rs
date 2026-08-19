use super::ThreadDpiAwareness;
use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, mpsc};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateSolidBrush, DEFAULT_GUI_FONT, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetStockObject,
    InflateRect, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_MENU, VK_RWIN, VK_TAB};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GA_ROOT,
    GW_HWNDNEXT, GW_HWNDPREV, GetAncestor, GetClientRect, GetForegroundWindow, GetSystemMetrics,
    GetWindow, IsIconic, IsWindow, IsWindowVisible, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_INJECTED,
    LLMHF_INJECTED, LWA_ALPHA, LWA_COLORKEY, MSG, MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW,
    RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

const OUTLINE_KEY: COLORREF = 0x00ff00ff;
const OUTLINE_COLOR: COLORREF = 0x000b9ef5;
const OUTLINE_WIDTH: usize = 3;
const BADGE_TEXT: &str = concat!("Renium ", env!("CARGO_PKG_VERSION"));

static KEYBOARD_TARGET: AtomicIsize = AtomicIsize::new(0);
static VIEWPORT: AtomicIsize = AtomicIsize::new(0);
static SHIELD_WINDOW: AtomicIsize = AtomicIsize::new(0);
static YIELDING: AtomicBool = AtomicBool::new(false);

pub(super) struct InputShield {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for InputShield {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) struct MouseYield {
    shield: isize,
}

impl Drop for MouseYield {
    fn drop(&mut self) {
        if self.shield == 0 {
            return;
        }
        let viewport = VIEWPORT.load(Ordering::Acquire);
        let top = KEYBOARD_TARGET.load(Ordering::Acquire);
        if viewport_rect(viewport as HWND, top as HWND).is_some() {
            unsafe { ShowWindow(self.shield as HWND, SW_SHOWNOACTIVATE) };
        }
        YIELDING.store(false, Ordering::Release);
    }
}

pub(super) fn yield_mouse(viewport: isize, message: u32) -> MouseYield {
    let is_mouse = matches!(
        message,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MOUSEWHEEL
    );
    let shield = if is_mouse && VIEWPORT.load(Ordering::Acquire) == viewport {
        SHIELD_WINDOW.load(Ordering::Acquire)
    } else {
        0
    };
    if shield != 0 {
        YIELDING.store(true, Ordering::Release);
        unsafe { ShowWindow(shield as HWND, SW_HIDE) };
    }
    MouseYield { shield }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> isize {
    if code >= 0 {
        let target = KEYBOARD_TARGET.load(Ordering::Acquire);
        let input = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let system_key = input.vkCode == VK_MENU as u32
            || input.vkCode == VK_LWIN as u32
            || input.vkCode == VK_RWIN as u32
            || input.vkCode == VK_TAB as u32 && input.flags & LLKHF_ALTDOWN != 0;
        if target != 0
            && input.flags & LLKHF_INJECTED == 0
            && !system_key
            && unsafe { GetForegroundWindow() } as isize == target
        {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> isize {
    let message = wparam as u32;
    if code >= 0
        && matches!(
            message,
            WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONUP
                | WM_MBUTTONDOWN
                | WM_MBUTTONUP
                | WM_XBUTTONDOWN
                | WM_XBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        )
    {
        let input = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        let viewport = VIEWPORT.load(Ordering::Acquire);
        let top = KEYBOARD_TARGET.load(Ordering::Acquire);
        if input.flags & LLMHF_INJECTED == 0
            && top != 0
            && unsafe { GetForegroundWindow() } as isize == top
            && viewport_rect(viewport as HWND, top as HWND).is_some_and(|rect| {
                input.pt.x >= rect.left
                    && input.pt.x < rect.right
                    && input.pt.y >= rect.top
                    && input.pt.y < rect.bottom
            })
        {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn viewport_rect(viewport: HWND, top: HWND) -> Option<RECT> {
    if unsafe { IsWindow(viewport) } == 0
        || unsafe { IsWindowVisible(viewport) } == 0
        || unsafe { IsWindowVisible(top) } == 0
        || unsafe { IsIconic(top) } != 0
    {
        return None;
    }
    let mut client: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(viewport, &mut client) } == 0 {
        return None;
    }
    let mut start = POINT {
        x: client.left,
        y: client.top,
    };
    let mut end = POINT {
        x: client.right,
        y: client.bottom,
    };
    if unsafe { ClientToScreen(viewport, &mut start) } == 0
        || unsafe { ClientToScreen(viewport, &mut end) } == 0
    {
        return None;
    }
    let screen_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let screen_top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let screen_right = screen_left + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let screen_bottom = screen_top + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (start.x < end.x
        && start.y < end.y
        && start.x < screen_right
        && end.x > screen_left
        && start.y < screen_bottom
        && end.y > screen_top)
        .then_some(RECT {
            left: start.x,
            top: start.y,
            right: end.x,
            bottom: end.y,
        })
}

fn clear_state() {
    SHIELD_WINDOW.store(0, Ordering::Release);
    VIEWPORT.store(0, Ordering::Release);
    KEYBOARD_TARGET.store(0, Ordering::Release);
    YIELDING.store(false, Ordering::Release);
}

unsafe extern "system" fn outline_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if message == WM_ERASEBKGND {
        return 1;
    }
    if message != WM_PAINT {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
    let dc = unsafe { BeginPaint(window, &mut paint) };
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(window, &mut rect) };
    let background = unsafe { CreateSolidBrush(OUTLINE_KEY) };
    unsafe {
        FillRect(dc, &rect, background);
        DeleteObject(background as _);
    }
    let border = unsafe { CreateSolidBrush(OUTLINE_COLOR) };
    for _ in 0..OUTLINE_WIDTH {
        unsafe {
            FrameRect(dc, &rect, border);
            InflateRect(&mut rect, -1, -1);
        }
    }
    let mut badge = RECT {
        left: (rect.right - 124).max(rect.left),
        top: rect.top,
        right: rect.right,
        bottom: (rect.top + 28).min(rect.bottom),
    };
    let text = BADGE_TEXT.encode_utf16().collect::<Vec<_>>();
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    unsafe {
        FillRect(dc, &badge, border);
        let previous = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00ffffff);
        DrawTextW(
            dc,
            text.as_ptr(),
            text.len() as i32,
            &mut badge,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
        SelectObject(dc, previous);
        DeleteObject(border as _);
        EndPaint(window, &paint);
    }
    0
}

fn create_outline(module: isize) -> std::result::Result<HWND, String> {
    let class_name = "ReniumInputOutline\0".encode_utf16().collect::<Vec<_>>();
    let class = WNDCLASSW {
        lpfnWndProc: Some(outline_window_proc),
        hInstance: module as _,
        lpszClassName: class_name.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { RegisterClassW(&class) } == 0
        && std::io::Error::last_os_error().raw_os_error() != Some(1410)
    {
        return Err(format!(
            "Could not register the viewport outline: {}",
            std::io::Error::last_os_error()
        ));
    }
    let outline = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            module as _,
            std::ptr::null(),
        )
    };
    if outline.is_null() {
        return Err(format!(
            "Could not create the viewport outline: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { SetLayeredWindowAttributes(outline, OUTLINE_KEY, 255, LWA_COLORKEY) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe { DestroyWindow(outline) };
        return Err(format!("Could not display the viewport outline: {error}"));
    }
    Ok(outline)
}

fn place_above(shield: HWND, top: HWND, rect: RECT) -> bool {
    let above = unsafe { GetWindow(top, GW_HWNDPREV) };
    let (insert_after, flags) = if above == shield {
        (
            std::ptr::null_mut(),
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_NOZORDER | SWP_SHOWWINDOW,
        )
    } else {
        (
            above,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_SHOWWINDOW,
        )
    };
    unsafe {
        SetWindowPos(
            shield,
            insert_after,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            flags,
        ) != 0
    }
}

fn run(
    viewport: isize,
    top: isize,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<std::result::Result<(), String>>,
) {
    let _dpi = ThreadDpiAwareness::per_monitor_v2();
    let module = unsafe { GetModuleHandleW(std::ptr::null()) } as isize;
    let class = "STATIC\0".encode_utf16().collect::<Vec<_>>();
    let shield = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            module as _,
            std::ptr::null(),
        )
    };
    if shield.is_null() {
        let _ = ready.send(Err(format!(
            "Could not create the viewport input shield: {}",
            std::io::Error::last_os_error()
        )));
        return;
    }
    if unsafe { SetLayeredWindowAttributes(shield, 0, 1, LWA_ALPHA) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe { DestroyWindow(shield) };
        let _ = ready.send(Err(format!(
            "Could not make the viewport input shield transparent: {error}"
        )));
        return;
    }
    let outline = match create_outline(module) {
        Ok(outline) => outline,
        Err(error) => {
            unsafe { DestroyWindow(shield) };
            let _ = ready.send(Err(error));
            return;
        }
    };
    let owns_viewport = VIEWPORT
        .compare_exchange(0, viewport, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let owns_keyboard = owns_viewport
        && KEYBOARD_TARGET
            .compare_exchange(0, top, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    if !owns_keyboard {
        if owns_viewport {
            VIEWPORT.store(0, Ordering::Release);
        }
        unsafe {
            DestroyWindow(outline);
            DestroyWindow(shield);
        }
        let _ = ready.send(Err("Another viewport input shield is active".to_string()));
        return;
    }
    SHIELD_WINDOW.store(shield as isize, Ordering::Release);
    let keyboard = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    let mouse = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if keyboard.is_null() || mouse.is_null() {
        if !keyboard.is_null() {
            unsafe { UnhookWindowsHookEx(keyboard) };
        }
        if !mouse.is_null() {
            unsafe { UnhookWindowsHookEx(mouse) };
        }
        clear_state();
        unsafe {
            DestroyWindow(outline);
            DestroyWindow(shield);
        }
        let _ = ready.send(Err(format!(
            "Could not shield physical input: {}",
            std::io::Error::last_os_error()
        )));
        return;
    }
    let mut shown = false;
    let mut previous = None;
    if let Some(rect) = viewport_rect(viewport as HWND, top as HWND) {
        if !place_above(shield, top as HWND, rect) || !place_above(outline, shield, rect) {
            let error = std::io::Error::last_os_error();
            unsafe {
                UnhookWindowsHookEx(mouse);
                UnhookWindowsHookEx(keyboard);
            }
            clear_state();
            unsafe {
                DestroyWindow(outline);
                DestroyWindow(shield);
            }
            let _ = ready.send(Err(format!(
                "Could not align the viewport input shield: {error}"
            )));
            return;
        }
        previous = Some((rect.left, rect.top, rect.right, rect.bottom));
        shown = true;
    }
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Acquire) && unsafe { IsWindow(viewport as HWND) } != 0 {
        if !YIELDING.load(Ordering::Acquire) {
            if let Some(rect) = viewport_rect(viewport as HWND, top as HWND) {
                let next = unsafe { GetWindow(shield, GW_HWNDNEXT) };
                let outline_next = unsafe { GetWindow(outline, GW_HWNDNEXT) };
                let bounds = (rect.left, rect.top, rect.right, rect.bottom);
                if previous != Some(bounds)
                    || next != top as HWND
                    || outline_next != shield
                    || !shown
                {
                    place_above(shield, top as HWND, rect);
                    place_above(outline, shield, rect);
                    previous = Some(bounds);
                    shown = true;
                }
            } else if shown {
                unsafe {
                    ShowWindow(outline, SW_HIDE);
                    ShowWindow(shield, SW_HIDE);
                }
                shown = false;
                previous = None;
            }
        }
        let mut message: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        UnhookWindowsHookEx(mouse);
        UnhookWindowsHookEx(keyboard);
    }
    clear_state();
    unsafe {
        DestroyWindow(outline);
        DestroyWindow(shield);
    }
}

pub(super) fn input_shield(viewport: isize) -> Result<InputShield> {
    let top = unsafe { GetAncestor(viewport as HWND, GA_ROOT) };
    if top.is_null() || viewport_rect(viewport as HWND, top).is_none() {
        return Ok(InputShield {
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        });
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker_stop = Arc::clone(&stop);
    let top = top as isize;
    let worker = std::thread::spawn(move || run(viewport, top, worker_stop, ready_tx));
    match ready_rx.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(Ok(())) => Ok(InputShield {
            stop,
            worker: Some(worker),
        }),
        Ok(Err(error)) => {
            let _ = worker.join();
            bail!(error)
        }
        Err(_) => {
            stop.store(true, Ordering::Release);
            let _ = worker.join();
            bail!("Timed out creating the viewport input shield")
        }
    }
}
