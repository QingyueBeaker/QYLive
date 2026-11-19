//! Windows.Graphics.Capture（WGC）采集：进程/窗口级捕获，
//! 解决部分游戏独占全屏下 DXGI 桌面复制黑屏的问题（Win10 1903+ / Win11）。

use std::ffi::c_void;

use windows::core::{Interface, Result as WinResult};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use super::{VideoCapture, VideoFrame};
use crate::config::CaptureBackendKind;

pub struct WgcCapture {
    hwnd: isize,
    monitor: usize,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    frame_pool: Option<Direct3D11CaptureFramePool>,
    session: Option<GraphicsCaptureSession>,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    stride: usize,
    alive: bool,
    started: bool,
}

unsafe impl Send for WgcCapture {}

impl WgcCapture {
    pub fn new(hwnd: isize, monitor: usize) -> Self {
        Self {
            hwnd,
            monitor,
            device: None,
            context: None,
            frame_pool: None,
            session: None,
            staging: None,
            width: 0,
            height: 0,
            stride: 0,
            alive: false,
            started: false,
        }
    }

    fn init(&mut self) -> Result<(), String> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);

            // 目标：窗口句柄优先，否则按显示器序号
            let interop: IGraphicsCaptureItemInterop = windows::core::factory::<
                GraphicsCaptureItem,
                IGraphicsCaptureItemInterop,
            >()
            .map_err(|e| format!("WGC: 获取 interop 失败: {e}"))?;

            let (item, w, h) = if self.hwnd != 0 {
                let hwnd = windows::Win32::Foundation::HWND(self.hwnd as *mut c_void);
                let item: GraphicsCaptureItem = interop
                    .CreateForWindow::<HWND, GraphicsCaptureItem>(hwnd)
                    .map_err(|e| format!("WGC: CreateForWindow 失败: {e}"))?;
                let mut rc = windows::Win32::Foundation::RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = ((rc.right - rc.left).max(2) as u32) & !1;
                let h = ((rc.bottom - rc.top).max(2) as u32) & !1;
                (item, w, h)
            } else {
                let hmon = monitor_handle(self.monitor)?;
                let item: GraphicsCaptureItem = interop
                    .CreateForMonitor::<windows::Win32::Graphics::Gdi::HMONITOR, GraphicsCaptureItem>(hmon)
                    .map_err(|e| format!("WGC: CreateForMonitor 失败: {e}"))?;
                let size = item
                    .Size()
                    .map_err(|e| format!("WGC: 读取捕获尺寸失败: {e}"))?;
                (item, (size.Width.max(2) as u32) & !1, (size.Height.max(2) as u32) & !1)
            };

            // D3D11 设备 + WinRT 设备包装
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .map_err(|e| format!("WGC: 创建 D3D11 设备失败: {e}"))?;
            let device = device.ok_or_else(|| "WGC: D3D11 设备为空".to_string())?;
            let context = context.ok_or_else(|| "WGC: D3D11 上下文为空".to_string())?;

            let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
                .cast()
                .map_err(|e| format!("WGC: 获取 IDXGIDevice 失败: {e}"))?;
            let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .map_err(|e| format!("WGC: 设备互操作失败: {e}"))?;
            let d3d_device: IDirect3DDevice = inspectable
                .cast()
                .map_err(|e| format!("WGC: IDirect3DDevice 转换失败: {e}"))?;

            let size = SizeInt32 {
                Width: w as i32,
                Height: h as i32,
            };
            let pool = Direct3D11CaptureFramePool::Create(
                &d3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )
            .map_err(|e| format!("WGC: 创建帧池失败: {e}"))?;
            let session = pool
                .CreateCaptureSession(&item)
                .map_err(|e| format!("WGC: 创建捕获会话失败: {e}"))?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            session
                .StartCapture()
                .map_err(|e| format!("WGC: 启动捕获失败: {e}"))?;

            let mut td = D3D11_TEXTURE2D_DESC::default();
            td.Width = w;
            td.Height = h;
            td.Format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
            td.MipLevels = 1;
            td.ArraySize = 1;
            td.SampleDesc.Count = 1;
            td.Usage = D3D11_USAGE_STAGING;
            td.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&td, None, Some(&mut staging))
                .map_err(|e| format!("WGC: 创建 staging 纹理失败: {e}"))?;

            self.device = Some(device);
            self.context = Some(context);
            self.frame_pool = Some(pool);
            self.session = Some(session);
            self.staging = staging;
            self.width = w;
            self.height = h;
            self.stride = (w * 4) as usize;
            self.alive = true;
            let _: WinResult<()> = Ok(());
            log::info!("WGC 捕获已启动 {}x{}", w, h);
        }
        Ok(())
    }
}

/// 通过 DXGI 输出描述拿到对应显示器的 HMONITOR。
fn monitor_handle(index: usize) -> Result<windows::Win32::Graphics::Gdi::HMONITOR, String> {
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("WGC: 创建 DXGI 工厂失败: {e}"))?;
        let mut count = 0usize;
        let mut adapter_idx = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            adapter_idx += 1;
            let mut out_idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(out_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                if count == index {
                    let desc = output
                        .GetDesc()
                        .map_err(|e| format!("WGC: 读取输出信息失败: {e}"))?;
                    return Ok(desc.Monitor);
                }
                count += 1;
                out_idx += 1;
            }
        }
        Err(format!("WGC: 找不到显示器 {index}"))
    }
}

impl VideoCapture for WgcCapture {
    fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        self.init()?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }
        if let Some(pool) = self.frame_pool.take() {
            let _ = pool.Close();
        }
        self.staging = None;
        self.context = None;
        self.device = None;
        self.alive = false;
        self.started = false;
        Ok(())
    }

    fn get_frame(&mut self, timeout_ms: u32) -> Result<Option<VideoFrame>, String> {
        if !self.started {
            return Err("WGC 采集未启动".into());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        unsafe {
            loop {
                let pool = match &self.frame_pool {
                    Some(p) => p,
                    None => return Ok(None),
                };
                match pool.TryGetNextFrame() {
                    Ok(frame) => {
                        let surface = frame
                            .Surface()
                            .map_err(|e| format!("WGC: 获取 Surface 失败: {e}"))?;
                        let access: IDirect3DDxgiInterfaceAccess =
                            surface
                                .cast()
                                .map_err(|e| format!("WGC: Surface 互操作失败: {e}"))?;
                        let texture: ID3D11Texture2D = access
                            .GetInterface()
                            .map_err(|e| format!("WGC: 获取纹理失败: {e}"))?;
                        let frame_w = frame.ContentSize().map(|s| s.Width as u32).unwrap_or(self.width);
                        let frame_h = frame.ContentSize().map(|s| s.Height as u32).unwrap_or(self.height);
                        if frame_w != self.width || frame_h != self.height {
                            self.width = frame_w & !1;
                            self.height = frame_h & !1;
                            self.stride = (self.width * 4) as usize;
                        }
                        let staging = self
                            .staging
                            .as_ref()
                            .ok_or_else(|| "WGC: staging 纹理缺失".to_string())?;
                        let context = self
                            .context
                            .as_ref()
                            .ok_or_else(|| "WGC: 上下文缺失".to_string())?;
                        context.CopyResource(staging, &texture);
                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        context
                            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                            .map_err(|e| format!("WGC: Map 失败: {e}"))?;
                        let row_pitch = mapped.RowPitch as usize;
                        let ptr = mapped.pData as *const u8;
                        let mut data = Vec::with_capacity(self.stride * self.height as usize);
                        for row in 0..self.height as usize {
                            data.extend_from_slice(std::slice::from_raw_parts(
                                ptr.add(row * row_pitch),
                                self.stride,
                            ));
                        }
                        context.Unmap(staging, 0);
                        let _ = frame.Close();

                        return Ok(Some(VideoFrame {
                            width: self.width,
                            height: self.height,
                            stride: self.stride,
                            data,
                            ts_ms: crate::now_ms(),
                        }));
                    }
                    Err(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Ok(None);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.alive && self.started
    }

    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Wgc
    }

    fn describe(&self) -> String {
        if self.hwnd != 0 {
            format!("WGC 窗口捕获 (hwnd=0x{:X})", self.hwnd)
        } else {
            format!("WGC 显示器捕获 (显示器 {})", self.monitor)
        }
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
