//! DXGI Desktop Duplication 采集：Windows 上开销最低的桌面/游戏采集路径。
//! 直接拿到显卡里的桌面纹理并拷贝到 staging texture，CPU 只做一次内存拷贝。

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use super::{VideoCapture, VideoFrame};
use crate::config::CaptureBackendKind;

pub struct DxgiCapture {
    monitor: usize,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    stride: usize,
    alive: bool,
    started: bool,
    lost_count: u32,
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new(monitor: usize) -> Self {
        Self {
            monitor,
            device: None,
            context: None,
            duplication: None,
            staging: None,
            width: 0,
            height: 0,
            stride: 0,
            alive: false,
            started: false,
            lost_count: 0,
        }
    }

    fn init(&mut self) -> Result<(), String> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| format!("DXGI: 创建工厂失败: {e}"))?;

            // 找到目标显示器对应的 output
            let mut index = 0usize;
            let mut target = None;
            let mut adapter_count = 0u32;
            'outer: loop {
                let adapter = match factory.EnumAdapters1(adapter_count) {
                    Ok(a) => a,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(_) => break,
                };
                adapter_count += 1;
                let mut out_idx = 0u32;
                loop {
                    match adapter.EnumOutputs(out_idx) {
                        Ok(output) => {
                            if index == self.monitor {
                                target = Some((adapter.clone(), output));
                                break 'outer;
                            }
                            index += 1;
                            out_idx += 1;
                        }
                        Err(_) => break,
                    }
                }
            }
            let (adapter, output) =
                target.ok_or_else(|| format!("DXGI: 找不到显示器 {}", self.monitor))?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| format!("DXGI: output 不支持桌面复制: {e}"))?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                Some(&adapter.cast().map_err(|e| format!("DXGI: adapter cast: {e}"))?),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .map_err(|e| format!("DXGI: 创建 D3D11 设备失败: {e}"))?;

            let device = device.ok_or_else(|| "DXGI: D3D11 设备为空".to_string())?;
            let context = context.ok_or_else(|| "DXGI: D3D11 上下文为空".to_string())?;
            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DXGI: 启动桌面复制失败: {e}"))?;

            let desc = output.GetDesc().map_err(|e| format!("DXGI: 读取输出信息失败: {e}"))?;
            let rc = desc.DesktopCoordinates;
            self.width = ((rc.right - rc.left) as u32) & !1;
            self.height = ((rc.bottom - rc.top) as u32) & !1;

            // staging texture：GPU -> CPU 回读
            let mut td = D3D11_TEXTURE2D_DESC::default();
            td.Width = self.width;
            td.Height = self.height;
            td.Format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
            td.MipLevels = 1;
            td.ArraySize = 1;
            td.SampleDesc.Count = 1;
            td.Usage = D3D11_USAGE_STAGING;
            td.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            td.BindFlags = 0;
            td.MiscFlags = 0;

            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&td, None, Some(&mut staging))
                .map_err(|e| format!("DXGI: 创建 staging 纹理失败: {e}"))?;

            self.device = Some(device);
            self.context = Some(context);
            self.duplication = Some(duplication);
            self.staging = staging;
            self.stride = (self.width * 4) as usize;
            self.alive = true;
            log::info!(
                "DXGI 桌面复制已启动 显示器 {}, {}x{}",
                self.monitor,
                self.width,
                self.height
            );
        }
        Ok(())
    }
}

impl VideoCapture for DxgiCapture {
    fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        self.init()?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        self.duplication = None;
        self.staging = None;
        self.context = None;
        self.device = None;
        self.alive = false;
        self.started = false;
        Ok(())
    }

    fn get_frame(&mut self, timeout_ms: u32) -> Result<Option<VideoFrame>, String> {
        if !self.started {
            return Err("DXGI 采集未启动".into());
        }
        let dup = match &self.duplication {
            Some(d) => d,
            None => return Ok(None),
        };
        let device = self.device.as_ref().unwrap();
        let context = self.context.as_ref().unwrap();

        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let res = dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource);
            if let Err(e) = res {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if code == DXGI_ERROR_ACCESS_LOST {
                    // 分辨率变化 / 独占全屏切换：重建复制会话
                    self.lost_count += 1;
                    log::warn!("DXGI 桌面复制会话失效，重建中");
                    self.duplication = None;
                    let output1_rebuild = {
                        // 重新枚举并复制
                        let _ = &self.device;
                        true
                    };
                    if output1_rebuild {
                        let _ = self.stop();
                        let _ = self.init();
                        self.started = true;
                    }
                    return Ok(None);
                }
                return Err(format!("DXGI: AcquireNextFrame 失败: {e}"));
            }

            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| "DXGI: 帧资源为空".to_string())?
                .cast()
                .map_err(|e| format!("DXGI: 帧转纹理失败: {e}"))?;

            let staging = match &self.staging {
                Some(s) => s,
                None => {
                    let _ = dup.ReleaseFrame();
                    return Ok(None);
                }
            };
            context.CopyResource(staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("DXGI: Map 失败: {e}"))?;
            let row_pitch = mapped.RowPitch as usize;
            let data_ptr = mapped.pData as *const u8;
            let mut data = Vec::with_capacity(self.stride * self.height as usize);
            for row in 0..self.height as usize {
                let src = data_ptr.add(row * row_pitch);
                data.extend_from_slice(std::slice::from_raw_parts(src, self.stride));
            }
            context.Unmap(staging, 0);
            let _ = dup.ReleaseFrame();
            let _ = device;

            Ok(Some(VideoFrame {
                width: self.width,
                height: self.height,
                stride: self.stride,
                data,
                ts_ms: crate::now_ms(),
            }))
        }
    }

    fn is_alive(&self) -> bool {
        self.alive && self.started
    }

    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Dxgi
    }

    fn describe(&self) -> String {
        format!("DXGI 桌面复制 (显示器 {})", self.monitor)
    }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[allow(dead_code)]
fn _assert_send<T: Send>(_: T) {}
