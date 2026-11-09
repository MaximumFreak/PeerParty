//! The overlay as a window of its own over a windowed or borderless game: a layered,
//! topmost popup that never takes focus or clicks (input reaches the overlay through
//! input.rs), showing the rendered frame with per-pixel alpha. Exclusive-fullscreen
//! games draw the frame themselves instead (frame.rs and the hook's present.rs).

use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::raster::Canvas;

pub struct Layered {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u32,
    size: (usize, usize),
    shown: bool,
    /// Where the window last went: a move redraws all of it.
    pos: (i32, i32),
}

static CLASS: Once = Once::new();
const CLASS_NAME: &[u16] = &[
    b'l' as u16, b'a' as u16, b'n' as u16, b'b' as u16, b'r' as u16, b'i' as u16, b'd' as u16, b'g' as u16, b'e' as u16,
    b'-' as u16, b'o' as u16, b'v' as u16, b'e' as u16, b'r' as u16, b'l' as u16, b'a' as u16, b'y' as u16, 0,
];

unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCHITTEST {
        return HTTRANSPARENT as LRESULT;
    }
    if m == WM_MOUSEACTIVATE {
        return MA_NOACTIVATE as LRESULT;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

impl Layered {
    pub fn new() -> Option<Layered> {
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        CLASS.call_once(|| {
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: module,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: CLASS_NAME.as_ptr(),
            };
            unsafe { RegisterClassW(&wc) };
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS_NAME.as_ptr(),
                CLASS_NAME.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                module,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        let screen = unsafe { GetDC(std::ptr::null_mut()) };
        let dc = unsafe { CreateCompatibleDC(screen) };
        unsafe { ReleaseDC(std::ptr::null_mut(), screen) };
        Some(Layered { hwnd, dc, bitmap: std::ptr::null_mut(), bits: std::ptr::null_mut(), size: (0, 0), shown: false, pos: (i32::MIN, 0) })
    }

    fn ensure(&mut self, w: usize, h: usize) -> bool {
        if self.size == (w, h) && !self.bits.is_null() {
            return true;
        }
        if !self.bitmap.is_null() {
            unsafe { DeleteObject(self.bitmap) };
            self.bitmap = std::ptr::null_mut();
        }
        let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(h as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits = std::ptr::null_mut();
        let bmp = unsafe { CreateDIBSection(self.dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0) };
        if bmp.is_null() || bits.is_null() {
            return false;
        }
        unsafe { SelectObject(self.dc, bmp) };
        self.bitmap = bmp;
        self.bits = bits.cast();
        self.size = (w, h);
        true
    }

    /// Shows `canvas` with its top-left at screen pixel (x, y). Only the `dirty`
    /// rectangles ([x0, y0, x1, y1] in canvas pixels) changed since the last call, unless
    /// the window moved or changed size.
    pub fn present(&mut self, canvas: &Canvas, x: i32, y: i32, dirty: &[[i32; 4]]) {
        let resized = self.size != (canvas.w, canvas.h);
        if canvas.w == 0 || canvas.h == 0 || !self.ensure(canvas.w, canvas.h) {
            return;
        }
        let full = resized || !self.shown || self.pos != (x, y);
        let rects: Vec<[usize; 4]> = if full {
            vec![[0, 0, canvas.w, canvas.h]]
        } else {
            dirty.iter().map(|r| super::raster::clamp_rect(*r, canvas.w, canvas.h)).filter(|r| r[0] < r[2] && r[1] < r[3]).collect()
        };
        if rects.is_empty() {
            return;
        }
        for &[x0, y0, x1, y1] in &rects {
            for row in y0..y1 {
                let at = row * canvas.w + x0;
                unsafe { std::ptr::copy_nonoverlapping(canvas.px.as_ptr().add(at), self.bits.add(at), x1 - x0) };
            }
        }
        // Windows takes one rectangle: the one around them all.
        let [x0, y0, x1, y1] = if full {
            [0, 0, canvas.w, canvas.h]
        } else {
            let u = rects.iter().fold([0i32; 4], |acc, r| super::raster::union(acc, [r[0] as i32, r[1] as i32, r[2] as i32, r[3] as i32]));
            super::raster::clamp_rect(u, canvas.w, canvas.h)
        };
        let pos = POINT { x, y };
        let size = SIZE { cx: canvas.w as i32, cy: canvas.h as i32 };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let rect = RECT { left: x0 as i32, top: y0 as i32, right: x1 as i32, bottom: y1 as i32 };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: std::ptr::null_mut(),
            pptDst: &pos,
            psize: &size,
            hdcSrc: self.dc,
            pptSrc: &src,
            crKey: 0,
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: if full { std::ptr::null() } else { &rect },
        };
        unsafe {
            UpdateLayeredWindowIndirect(self.hwnd, &info);
            if full {
                SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
        self.pos = (x, y);
        self.shown = true;
    }

    pub fn hide(&mut self) {
        if self.shown {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.shown = false;
        }
    }
}

impl Drop for Layered {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            DeleteDC(self.dc);
        }
    }
}
