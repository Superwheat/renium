//! Reuse a top-down DIB for recording. Only documented client-window capture;
//! no desktop fallback, activation, window manipulation or Studio memory writes.
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    RestoreDC, SaveDC, SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClientRect, GetWindowThreadProcessId, IsIconic,
};

use super::{StudioWindow, ThreadDpiAwareness};

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, dc: HDC, flags: u32) -> i32;
}

const PW_CLIENTONLY: u32 = 1;

pub(crate) struct Capture {
    target: isize,
    root: isize,
    pid: u32,
    identity: String,
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
    length: usize,
}

impl Capture {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn in_background(&self) -> bool {
        let front = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(front, &mut owner) };
        owner != self.pid
    }

    pub(crate) fn new(window: &StudioWindow) -> Result<Self> {
        super::platform::verify_recording_target(&window.handle)?;
        let (pid, identity) = window
            .handle
            .recording_owner
            .clone()
            .context("Unverified recording owner")?;
        let target = window.handle.capture;
        let root = unsafe { GetAncestor(target as HWND, GA_ROOT) } as isize;
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(root as HWND, &mut owner) };
        ensure!(
            owner == pid,
            "The recording root no longer belongs to Studio"
        );
        let dc = unsafe {
            let source = GetDC(root as HWND);
            ensure!(!source.is_null(), "GetDC failed for capture target");
            let dc = CreateCompatibleDC(source);
            ReleaseDC(root as HWND, source);
            dc
        };
        ensure!(
            !dc.is_null(),
            "CreateCompatibleDC failed for capture target"
        );
        Ok(Self {
            target,
            root,
            pid,
            identity,
            dc,
            bitmap: null_mut(),
            original: null_mut(),
            bits: null_mut(),
            width: 0,
            height: 0,
            length: 0,
        })
    }

    fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        let length = frame_length(width, height)?;
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let bitmap =
            unsafe { CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        ensure!(
            !bitmap.is_null(),
            "CreateDIBSection failed for capture target"
        );
        if bits.is_null() {
            unsafe { DeleteObject(bitmap as _) };
            anyhow::bail!("CreateDIBSection omitted the capture pixels");
        }
        let previous = unsafe { SelectObject(self.dc, bitmap as _) };
        if previous.is_null() || previous as isize == -1 {
            unsafe { DeleteObject(bitmap as _) };
            anyhow::bail!("SelectObject failed for capture target");
        }
        if self.original.is_null() {
            self.original = previous;
        } else {
            unsafe { DeleteObject(self.bitmap as _) };
        }
        self.bitmap = bitmap;
        self.bits = bits.cast();
        self.width = width;
        self.height = height;
        self.length = length;
        Ok(())
    }

    /// Native BGRA pixels of the verified target's client area, with odd
    /// sizes cropped to even ones. The owning window is captured and the
    /// viewport cropped from it: client-only PrintWindow on the viewport
    /// child itself returns stale pixels.
    pub(crate) fn next(&mut self, stop: &AtomicBool) -> Result<(u32, u32, Vec<u8>)> {
        ensure!(!stop.load(Ordering::Relaxed), "Recording stopped");
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(self.target as HWND, &mut owner) };
        ensure!(
            owner == self.pid
                && crate::app::update::process_start_identity(self.pid).as_ref()
                    == Some(&self.identity),
            "Studio recording owner ended"
        );
        unsafe { GetWindowThreadProcessId(self.root as HWND, &mut owner) };
        ensure!(
            owner == self.pid,
            "The recording root no longer belongs to Studio"
        );
        ensure!(
            unsafe { GetAncestor(self.target as HWND, GA_ROOT) } as isize == self.root
                && unsafe { IsIconic(self.root as HWND) } == 0,
            "Recording window changed or minimized"
        );
        let _dpi = ThreadDpiAwareness::per_monitor_v2();
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetClientRect(self.root as HWND, &mut rect) } != 0,
            "GetClientRect failed for capture target"
        );
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        if (width, height) != (self.width, self.height) {
            self.resize(width, height)?;
        }
        let saved = unsafe { SaveDC(self.dc) };
        ensure!(saved != 0, "Could not save the capture DC");
        let printed = unsafe { PrintWindow(self.root as HWND, self.dc, PW_CLIENTONLY) };
        let restored = unsafe { RestoreDC(self.dc, saved) };
        ensure!(
            printed != 0 && restored != 0,
            "PrintWindow rejected the recording target or changed its DC"
        );
        ensure!(unsafe { GdiFlush() } != 0, "GDI capture did not finish");
        let source = unsafe { std::slice::from_raw_parts(self.bits, self.length) };
        let (x, y, even_width, even_height) = self.region()?;
        let mut pixels = vec![0; frame_length(even_width, even_height)?];
        for (source, target) in source
            .chunks_exact(width as usize * 4)
            .skip(y)
            .take(even_height as usize)
            .zip(pixels.chunks_exact_mut(even_width as usize * 4))
        {
            target.copy_from_slice(&source[x * 4..(x + even_width as usize) * 4]);
        }
        Ok((even_width as u32, even_height as u32, pixels))
    }

    fn region(&self) -> Result<(usize, usize, i32, i32)> {
        if self.target == self.root {
            return Ok((0, 0, self.width & !1, self.height & !1));
        }
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let mut target: POINT = unsafe { std::mem::zeroed() };
        let mut root = target;
        ensure!(
            unsafe { GetClientRect(self.target as HWND, &mut rect) } != 0
                && unsafe { ClientToScreen(self.target as HWND, &mut target) } != 0
                && unsafe { ClientToScreen(self.root as HWND, &mut root) } != 0,
            "Could not locate the recording viewport"
        );
        let x = target
            .x
            .checked_sub(root.x)
            .context("Viewport is outside its window")?;
        let y = target
            .y
            .checked_sub(root.y)
            .context("Viewport is outside its window")?;
        ensure!(
            x >= 0
                && y >= 0
                && x < self.width
                && y < self.height
                && rect.right > 0
                && rect.bottom > 0
                && rect.right <= self.width - x
                && rect.bottom <= self.height - y,
            "Recording viewport is outside its window"
        );
        Ok((x as usize, y as usize, rect.right & !1, rect.bottom & !1))
    }
}

fn frame_length(width: i32, height: i32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "Capture target has no client area");
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("Capture frame is too large")
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            if !self.original.is_null() {
                SelectObject(self.dc, self.original);
            }
            DeleteDC(self.dc);
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap as _);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{BITMAP, GetObjectW};

    #[test]
    fn a_resized_capture_frees_the_bitmap_it_replaces_and_drop_frees_the_rest() {
        let live = |bitmap: HBITMAP| {
            let mut described: BITMAP = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<BITMAP>() as i32;
            unsafe { GetObjectW(bitmap as _, size, (&raw mut described).cast()) != 0 }
        };
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        assert!(!dc.is_null());
        let mut capture = Capture {
            target: 0,
            root: 0,
            pid: 0,
            identity: String::new(),
            dc,
            bitmap: null_mut(),
            original: null_mut(),
            bits: null_mut(),
            width: 0,
            height: 0,
            length: 0,
        };
        for (width, height) in [(64, 32), (97, 65), (128, 128), (64, 32)] {
            let replaced = capture.bitmap;
            capture.resize(width, height).unwrap();
            assert!(!capture.bits.is_null());
            assert!(live(capture.bitmap));
            assert!(replaced.is_null() || !live(replaced));
        }
        let bitmap = capture.bitmap;
        assert!(capture.resize(0, 32).is_err());
        assert_eq!(capture.bitmap, bitmap);
        assert!(live(bitmap));
        assert!(capture.next(&AtomicBool::new(true)).is_err());
        assert!(capture.next(&AtomicBool::new(false)).is_err());
        assert!(live(bitmap));
        drop(capture);
        assert!(!live(bitmap));
        assert_eq!(unsafe { SaveDC(dc) }, 0, "the capture's DC is deleted");
    }
}
