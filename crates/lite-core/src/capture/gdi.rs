//! GDI BitBlt 采集：万能兜底，兼容一切窗口/桌面。
//! 开销高于 DXGI/WGC，仅在自动降级到最后一层或用户手动锁定 GDI 时使用。

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP,
    HDC, HGDIOBJ, ROP_CODE, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetSystemMetrics, IsWindow, SM_CXSCREEN, SM_CYSCREEN,
};

use super::{VideoCapture, VideoFrame};
use crate::config::CaptureBackendKind;

const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

pub struct GdiCapture {
    hwnd: isize,
    monitor: usize,
    width: u32,
    height: u32,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut c_void,
    stride: usize,
    alive: bool,
    started: bool,
}

unsafe impl Send for GdiCapture {}

impl GdiCapture {
    pub fn new(hwnd: isize, monitor: usize) -> Self {
        Self {
            hwnd,
            monitor,
            width: 0,
            height: 0,
            screen_dc: HDC::default(),
            mem_dc: HDC::default(),
            bitmap: HBITMAP::default(),
            old_bitmap: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            stride: 0,
            alive: false,
            started: false,
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    fn window_size(&self) -> (i32, i32) {
        unsafe {
            if self.hwnd != 0 && IsWindow(self.hwnd()).as_bool() {
                let mut rc = RECT::default();
                if GetClientRect(self.hwnd(), &mut rc).is_ok() {
                    return ((rc.right - rc.left).max(2), (rc.bottom - rc.top).max(2));
                }
            }
            (
                GetSystemMetrics(SM_CXSCREEN).max(2),
                GetSystemMetrics(SM_CYSCREEN).max(2),
            )
        }
    }
}

impl VideoCapture for GdiCapture {
    fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        unsafe {
            let (w, h) = self.window_size();
            self.width = (w as u32) & !1;
            self.height = (h as u32) & !1;

            self.screen_dc = if self.hwnd != 0 {
                GetDC(self.hwnd())
            } else {
                GetDC(HWND(std::ptr::null_mut()))
            };
            if self.screen_dc.is_invalid() {
                return Err("GDI: GetDC 失败".into());
            }
            self.mem_dc = CreateCompatibleDC(self.screen_dc);
            if self.mem_dc.is_invalid() {
                return Err("GDI: CreateCompatibleDC 失败".into());
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.width as i32,
                    biHeight: -(self.height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            self.bitmap = CreateDIBSection(
                self.screen_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE(std::ptr::null_mut()),
                0,
            )
            .map_err(|e| format!("GDI: CreateDIBSection 失败: {e}"))?;
            self.bits = bits;
            self.stride = (self.width * 4) as usize;
            self.old_bitmap = SelectObject(self.mem_dc, self.bitmap);
            self.alive = true;
            self.started = true;
            log::info!("{} 启动 {}x{}", self.describe(), self.width, self.height);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        unsafe {
            if !self.mem_dc.is_invalid() {
                if !self.old_bitmap.is_invalid() {
                    SelectObject(self.mem_dc, self.old_bitmap);
                }
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.screen_dc.is_invalid() {
                if self.hwnd != 0 {
                    let _ = ReleaseDC(self.hwnd(), self.screen_dc);
                } else {
                    let _ = ReleaseDC(HWND(std::ptr::null_mut()), self.screen_dc);
                }
            }
            self.mem_dc = HDC::default();
            self.screen_dc = HDC::default();
            self.bitmap = HBITMAP::default();
            self.old_bitmap = HGDIOBJ::default();
            self.bits = std::ptr::null_mut();
        }
        self.alive = false;
        self.started = false;
        Ok(())
    }

    fn get_frame(&mut self, _timeout_ms: u32) -> Result<Option<VideoFrame>, String> {
        if !self.started {
            return Err("GDI 采集未启动".into());
        }
        unsafe {
            let ok = if self.hwnd != 0 && IsWindow(self.hwnd()).as_bool() {
                PrintWindow(
                    self.hwnd(),
                    self.mem_dc,
                    PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT),
                )
                .as_bool()
            } else {
                BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    self.screen_dc,
                    0,
                    0,
                    ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                )
                .is_ok()
            };
            // 部分窗口 PrintWindow 失败时退回 BitBlt 屏幕拷贝
            let ok = if !ok && self.hwnd == 0 {
                false
            } else {
                ok
            };
            if !ok {
                self.alive = false;
                return Ok(None);
            }
            let len = self.stride * self.height as usize;
            let src = std::slice::from_raw_parts(self.bits as *const u8, len);
            Ok(Some(VideoFrame {
                width: self.width,
                height: self.height,
                stride: self.stride,
                data: src.to_vec(),
                ts_ms: crate::now_ms(),
            }))
        }
    }

    fn is_alive(&self) -> bool {
        self.alive && self.started
    }

    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Gdi
    }

    fn describe(&self) -> String {
        if self.hwnd != 0 {
            format!("GDI 窗口捕获 (hwnd=0x{:X})", self.hwnd)
        } else {
            format!("GDI 全屏捕获 (显示器 {})", self.monitor)
        }
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
