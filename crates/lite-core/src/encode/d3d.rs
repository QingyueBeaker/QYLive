//! D3D11 纹理路径：硬件编码器（NVENC / Intel QSV / AMF）只接受 GPU 纹理输入，
//! 系统内存 NV12 会被直接拒收（表现为“编码器持续拒绝输入”）。
//!
//! 这里负责：
//! 1. 建 D3D11 设备与 `IMFDXGIDeviceManager`（交给编码器 MFT）
//! 2. 建一张 NV12 的默认用途纹理
//! 3. 每帧把 CPU 的 NV12 数据上传到纹理，再包成 IMFSample 给编码器

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIResource,
};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFSample, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateSample,
};

pub struct GpuPath {    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub texture: ID3D11Texture2D,
    /// 必须一直持有：MFT 会拿 token 回查这个管理器，提前释放会导致访问冲突
    pub manager: IMFDXGIDeviceManager,
    pub token: u32,
    pub width: u32,
    pub height: u32,
}

/// 按编码器厂商挑 DXGI 适配器（名称子串匹配），匹配不到就退回第一个非软件适配器。
fn pick_adapter(hint: &str) -> Option<IDXGIAdapter1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let hint = hint.to_ascii_lowercase();
        let mut fallback: Option<IDXGIAdapter1> = None;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & 2 != 0 {
                // DXGI_ADAPTER_FLAG_SOFTWARE：跳过软件渲染适配器
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]).to_ascii_lowercase();
            if !hint.is_empty() && name.contains(&hint) {
                return Some(adapter);
            }
            if fallback.is_none() {
                fallback = Some(adapter);
            }
        }
        fallback
    }
}

/// 一个最小的 “GPU 画布”：设备 + 上下文 + 一张 NV12 纹理。
/// NVENC 直连（nvEncodeAPI）只需要这三样，不需要 IMFDXGIDeviceManager。
pub struct GpuCanvas {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub texture: ID3D11Texture2D,
}

pub fn gpu_canvas(width: u32, height: u32, hint: &str) -> Result<GpuCanvas, String> {
    unsafe {
        let adapter = pick_adapter(hint);
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        let adapter_iface: Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> =
            adapter.as_ref().and_then(|a| a.cast().ok());
        let (adapter_arg, driver_type) = match adapter_iface.as_ref() {
            Some(a) => (Some(a), D3D_DRIVER_TYPE_UNKNOWN),
            None => (None, D3D_DRIVER_TYPE_HARDWARE),
        };
        D3D11CreateDevice(
            adapter_arg,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .map_err(|e| format!("创建 D3D11 设备失败: {e}"))?;
        let device = device.ok_or("D3D11 设备为空")?;
        let context = context.ok_or("D3D11 上下文为空")?;
        if let Ok(mt) = device.cast::<windows::Win32::Graphics::Direct3D10::ID3D10Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| format!("创建 NV12 纹理失败: {e}"))?;
        let texture = texture.ok_or("NV12 纹理为空")?;
        Ok(GpuCanvas {
            device,
            context,
            texture,
        })
    }
}

impl GpuPath {
    /// `hint`：编码器厂商标识（例如 "intel" / "nvidia" / "amd"）。
    /// 笔记本上默认 D3D 适配器往往是独显，而 QSV 的 MFT 只认 Intel 集显，
    /// 设备管理器给错适配器会让 MFT 在 SET_D3D_MANAGER 时直接崩溃，所以必须按厂商挑适配器。
    pub fn new(width: u32, height: u32, hint: &str) -> Result<Self, String> {
        unsafe {
            let adapter = pick_adapter(hint);
            if let Some(a) = adapter.as_ref() {
                if let Ok(desc) = a.GetDesc1() {
                    let name: String = String::from_utf16_lossy(
                        &desc.Description[..desc
                            .Description
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(desc.Description.len())],
                    );
                    log::info!("GPU 编码使用适配器: {name}");
                }
            }
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            let adapter_iface: Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> =
                adapter.as_ref().and_then(|a| a.cast().ok());
            let (adapter_arg, driver_type) = match adapter_iface.as_ref() {
                Some(a) => (Some(a), D3D_DRIVER_TYPE_UNKNOWN),
                None => (None, D3D_DRIVER_TYPE_HARDWARE),
            };
            D3D11CreateDevice(
                adapter_arg,
                driver_type,
                HMODULE::default(),
                // 视频编码器要求设备带 VIDEO_SUPPORT 标志，否则部分硬件 MFT 会直接崩
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .map_err(|e| format!("创建 D3D11 设备失败: {e}"))?;
            let device = device.ok_or("D3D11 设备为空")?;
            let context = context.ok_or("D3D11 上下文为空")?;

            // 编码器会在自己的线程里使用这张设备，必须开多线程保护
            if let Ok(mt) = device.cast::<windows::Win32::Graphics::Direct3D10::ID3D10Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| format!("创建 D3D 设备管理器失败: {e}"))?;
            let manager = manager.ok_or("D3D 设备管理器为空")?;
            manager
                .ResetDevice(
                    &device
                        .cast::<windows::core::IUnknown>()
                        .map_err(|e| e.to_string())?,
                    token,
                )
                .map_err(|e| format!("绑定 D3D 设备失败: {e}"))?;

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| format!("创建 NV12 纹理失败: {e}"))?;
            let texture = texture.ok_or("NV12 纹理为空")?;

            Ok(Self {
                device,
                context,
                texture,
                manager,
                token,
                width,
                height,
            })
        }
    }

    /// 把一帧 NV12（Y 平面 + UV 平面，行宽 = width）上传到纹理并包成 IMFSample。
    pub fn make_sample(&self, nv12: &[u8]) -> Result<IMFSample, String> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_size = w * h;
        let uv_size = w * (h / 2);
        if nv12.len() < y_size + uv_size {
            return Err(format!(
                "帧数据长度不足：需要 {}，实际 {}",
                y_size + uv_size,
                nv12.len()
            ));
        }
        unsafe {
            // subresource 0 = Y 平面，subresource 1 = UV 平面（NV12 的两个平面）
            self.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                nv12.as_ptr() as *const std::ffi::c_void,
                self.width,
                0,
            );
            self.context.UpdateSubresource(
                &self.texture,
                1,
                None,
                nv12[y_size..].as_ptr() as *const std::ffi::c_void,
                self.width,
                0,
            );

            let resource: IDXGIResource = self
                .texture
                .cast()
                .map_err(|e| format!("纹理转 IDXGIResource 失败: {e}"))?;
            let unk: windows::core::IUnknown = resource
                .cast()
                .map_err(|e| format!("IDXGIResource 转 IUnknown 失败: {e}"))?;

            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &unk, 0, false)
                .map_err(|e| format!("纹理包成 DXGI 缓冲区失败: {e}"))?;

            let sample: IMFSample = MFCreateSample().map_err(|e| format!("创建采样失败: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("挂载纹理缓冲区失败: {e}"))?;
            Ok(sample)
        }
    }
}


/// BGRA → NV12 的 GPU 转换器（D3D11 Video Processor）。
///
/// 为什么需要：CPU 侧 `bgra_to_nv12` 实测 1920x1080 约 3.7 ms/帧，60fps 下等于吃掉
/// 单核 22%。这里把「色彩空间转换 + 缩放」整件事交给 GPU 的视频处理器，
/// CPU 只做一次行对齐的内存上传（D3D11 驱动内部走 PCIe 异步拷贝）。
pub struct BgraConverter {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    in_tex: ID3D11Texture2D,
    in_view: ID3D11VideoProcessorInputView,
    out_view: ID3D11VideoProcessorOutputView,
    pub src_w: u32,
    pub src_h: u32,
    pub dst_w: u32,
    pub dst_h: u32,
}

unsafe impl Send for BgraConverter {}

impl BgraConverter {
    /// `out_tex`：目标 NV12 纹理（就是交给 NVENC 的那张）。
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        out_tex: &ID3D11Texture2D,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        fps: u32,
    ) -> Result<Self, String> {
        unsafe {
            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| format!("取 ID3D11VideoDevice 失败: {e}"))?;
            let video_context: ID3D11VideoContext = context
                .cast()
                .map_err(|e| format!("取 ID3D11VideoContext 失败: {e}"))?;

            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: fps.max(1),
                    Denominator: 1,
                },
                InputWidth: src_w,
                InputHeight: src_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: fps.max(1),
                    Denominator: 1,
                },
                OutputWidth: dst_w,
                OutputHeight: dst_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| format!("创建视频处理器枚举器失败: {e}"))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| format!("创建视频处理器失败: {e}"))?;

            // 输入纹理：BGRA8，默认用途，供视频处理器读取
            let in_desc = D3D11_TEXTURE2D_DESC {
                Width: src_w,
                Height: src_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut in_tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&in_desc, None, Some(&mut in_tex))
                .map_err(|e| format!("创建 BGRA 输入纹理失败: {e}"))?;
            let in_tex = in_tex.ok_or("BGRA 输入纹理为空")?;

            let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            video_device
                .CreateVideoProcessorInputView(
                    &in_tex,
                    &enumerator,
                    &in_view_desc,
                    Some(&mut in_view),
                )
                .map_err(|e| format!("创建输入视图失败: {e}"))?;
            let in_view = in_view.ok_or("输入视图为空")?;

            let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device
                .CreateVideoProcessorOutputView(
                    out_tex,
                    &enumerator,
                    &out_view_desc,
                    Some(&mut out_view),
                )
                .map_err(|e| format!("创建输出视图失败: {e}"))?;
            let out_view = out_view.ok_or("输出视图为空")?;

            // 输入按 RGB 全范围，输出按 BT.601 limited range（与 CPU 路径的整数公式一致）
            let stream_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &stream_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &out_cs);

            let src_rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: src_w as i32,
                bottom: src_h as i32,
            };
            let dst_rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: dst_w as i32,
                bottom: dst_h as i32,
            };
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&src_rect),
            );
            video_context.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&dst_rect),
            );
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                video_context,
                processor,
                in_tex,
                in_view,
                out_view,
                src_w,
                src_h,
                dst_w,
                dst_h,
            })
        }
    }

    /// 上传一帧 BGRA 并在 GPU 上转成 NV12，结果落在 `out_tex`。
    pub fn convert(&self, bgra: &[u8], stride: usize) -> Result<(), String> {
        let need = stride * self.src_h as usize;
        if bgra.len() < need {
            return Err(format!("BGRA 帧不足：需要 {need}，实际 {}", bgra.len()));
        }
        unsafe {
            self.context.UpdateSubresource(
                &self.in_tex,
                0,
                None,
                bgra.as_ptr() as *const std::ffi::c_void,
                stride as u32,
                0,
            );

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(self.in_view.clone())),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.out_view, 0, &[stream])
                .map_err(|e| format!("视频处理器 Blt 失败: {e}"))?;
            // 立刻提交，保证 NVENC 映射时数据已就绪
            self.context.Flush();
            Ok(())
        }
    }
}
