//! NVENC 直连编码器（nvEncodeAPI64.dll）。
//!
//! 为什么不走 Media Foundation 的 “NVIDIA H.264 Encoder MFT”：
//! 那套 MFT 在本机（Optimus 双显卡笔记本）激活可以成功，但一收到
//! `MFT_MESSAGE_SET_D3D_MANAGER` 就直接访问冲突崩溃（Intel QSV 的 MFT 也一样），
//! 这是驱动封装层的问题，客户端无法补救。
//! 而 NVENC SDK 本身是好的（官方直播客户端用的就是它），所以这里直接调 nvEncodeAPI：
//!
//! 1. `NvEncodeAPICreateInstance` 拿到函数表（mini-nvcodec 负责 dlopen 与版本阶梯）
//! 2. `nvEncOpenEncodeSessionEx`（deviceType = DIRECTX，设备 = ID3D11Device）
//! 3. 建 session 配置（H.264 / NV12 / 低延迟 / GOP / 码率控制）
//! 4. 注册一张 NV12 的 D3D11 纹理，逐帧上传后编码
//! 5. 输出位流是 Annex-B，交给 `flv::annexb_to_avcc` 规整成 AVCC

use std::ffi::c_void;
use std::ptr;

use mini_nvcodec::guids::{
    NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_MAIN_GUID, NV_ENC_PRESET_P4_GUID,
};
use mini_nvcodec::nvenc_sys::*;
use mini_nvcodec::nvenc_ver::NvencApiVersion;
use mini_nvcodec::{try_init_nvenc, NvencApi};
use windows::core::Interface;

use super::h264::{EncodedVideo, MfH264Encoder};
use crate::config::RateControl;
use crate::config::VideoConfig;

pub struct NvencH264Encoder {
    api: &'static NvencApi,
    rung: NvencApiVersion,
    session: *mut c_void,
    canvas: super::d3d::GpuCanvas,
    registered: NV_ENC_REGISTERED_PTR,
    bitstream: NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,
    /// 关键帧间隔（帧数），已写入 config，仅用于诊断
    #[allow(dead_code)]
    gop: u32,
    force_key: bool,
    extradata: Option<Vec<u8>>,
    frame_idx: u32,
    /// BGRA 输入时的 GPU 转换器（懒创建；源分辨率变化会重建）
    conv: Option<super::d3d::BgraConverter>,
    fps: u32,
}

unsafe impl Send for NvencH264Encoder {}

fn nv_err(what: &str, status: NVENCSTATUS) -> String {
    format!("{what} 失败: {status:?}")
}

impl NvencH264Encoder {
    /// 本机是否具备可用的 NVENC（有 NVIDIA 驱动 + nvEncodeAPI64.dll）。
    pub fn available() -> bool {
        try_init_nvenc().is_some()
    }

    pub fn new(cfg: &VideoConfig) -> Result<Self, String> {
        let api = try_init_nvenc().ok_or("nvEncodeAPI 不可用（没有 NVIDIA 驱动或不在 NVIDIA 显卡上）")?;

        // 用加载器已经协商成功的那一档：函数表与 apiVersion 必须完全一致，
        // 自己另选一档会得到 NV_ENC_ERR_INVALID_VERSION(15)。
        let rung = api.created_with;

        // 只认 NVIDIA 适配器上的 D3D 设备
        let canvas = super::d3d::gpu_canvas(cfg.width, cfg.height, "nvidia")?;

        unsafe {
            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = rung.open_encode_session_ex_params_ver();
            open.deviceType = NV_ENC_DEVICE_TYPE::DIRECTX;
            open.device = canvas.device.as_raw();
            open.apiVersion = rung.nvencapi_version();
            let mut session: *mut c_void = ptr::null_mut();
            let st = (api.nvEncOpenEncodeSessionEx)(&mut open, &mut session);
            if st != NVENCSTATUS::SUCCESS {
                return Err(nv_err("打开 NVENC 会话", st));
            }

            // 用 P4（偏向质量）预设当基线，再按我们的直播参数覆盖
            let mut preset: NV_ENC_PRESET_CONFIG = std::mem::zeroed();
            preset.version = rung.preset_config_ver();
            preset.presetCfg.version = rung.config_ver();
            let st = (api.nvEncGetEncodePresetConfigEx)(
                session,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO::LOW_LATENCY,
                &mut preset,
            );
            if st != NVENCSTATUS::SUCCESS {
                (api.nvEncDestroyEncoder)(session);
                return Err(nv_err("读取 NVENC 预设配置", st));
            }

            let mut config = preset.presetCfg;
            config.version = rung.config_ver();
            // 直播必须关闭 B 帧：IBP 之外的结构会让平台侧解码抖动
            config.frameIntervalP = 1;
            config.profileGUID = NV_ENC_H264_PROFILE_MAIN_GUID;
            let gop = cfg.gop_seconds.saturating_mul(cfg.fps).max(1);
            config.gopLength = gop;

            // 码率控制
            let bitrate = cfg.bitrate_kbps.saturating_mul(1000);
            config.rcParams.version = rung.rc_params_ver();
            match cfg.rate_control {
                RateControl::Cbr => {
                    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::CBR;
                    config.rcParams.averageBitRate = bitrate;
                    config.rcParams.maxBitRate = bitrate;
                }
                RateControl::Abr => {
                    // NVENC 的 VBR + 平均/峰值 就是 ABR 行为
                    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::VBR;
                    config.rcParams.averageBitRate = bitrate;
                    config.rcParams.maxBitRate = bitrate.saturating_mul(2);
                }
                RateControl::Vbr => {
                    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::VBR;
                    config.rcParams.averageBitRate = (bitrate as f64 * 0.8) as u32;
                    config.rcParams.maxBitRate = bitrate.saturating_mul(3);
                }
            }
            // 1 秒 VBV，避免瞬时峰值打爆上行
            config.rcParams.vbvBufferSize = bitrate;
            config.rcParams.vbvInitialDelay = bitrate;
            // P4 预设默认走“目标质量(CQ)”模式：targetQuality 非 0 时 NVENC 会忽略
            // averageBitRate / maxBitRate，按固定质量出码（实测 8000kbps 目标跑出 38Mbps）。
            // 直播必须按码率控制，所以这里把它清零。
            config.rcParams.targetQuality = 0;
            config.rcParams.targetQualityLSB = 0;

            // H.264 细节：每个 IDR 都重复 SPS/PPS，方便平台中途接入
            let mut h264 = config.encodeCodecConfig.h264Config;
            h264.idrPeriod = gop;
            h264.set_repeatSPSPPS(1);
            config.encodeCodecConfig.h264Config = h264;

            let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
            init.version = rung.initialize_params_ver();
            init.encodeGUID = NV_ENC_CODEC_H264_GUID;
            init.presetGUID = NV_ENC_PRESET_P4_GUID;
            init.encodeWidth = cfg.width;
            init.encodeHeight = cfg.height;
            init.darWidth = cfg.width;
            init.darHeight = cfg.height;
            init.frameRateNum = cfg.fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.tuningInfo = NV_ENC_TUNING_INFO::LOW_LATENCY;
            init.maxEncodeWidth = cfg.width;
            init.maxEncodeHeight = cfg.height;
            init.encodeConfig = &mut config;
            let st = (api.nvEncInitializeEncoder)(session, &mut init);
            if st != NVENCSTATUS::SUCCESS {
                (api.nvEncDestroyEncoder)(session);
                return Err(nv_err("初始化 NVENC 编码器", st));
            }

            let mut bsb: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            bsb.version = rung.create_bitstream_buffer_ver();
            let st = (api.nvEncCreateBitstreamBuffer)(session, &mut bsb);
            if st != NVENCSTATUS::SUCCESS {
                (api.nvEncDestroyEncoder)(session);
                return Err(nv_err("创建位流缓冲区", st));
            }

            let mut reg: NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
            reg.version = rung.register_resource_ver();
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::DIRECTX;
            reg.width = cfg.width;
            reg.height = cfg.height;
            reg.pitch = 0;
            reg.subResourceIndex = 0;
            reg.resourceToRegister = canvas.texture.as_raw();
            reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV12;
            reg.bufferUsage = NV_ENC_BUFFER_USAGE::INPUT_IMAGE;
            let st = (api.nvEncRegisterResource)(session, &mut reg);
            if st != NVENCSTATUS::SUCCESS {
                let _ = (api.nvEncDestroyBitstreamBuffer)(session, bsb.bitstreamBuffer);
                (api.nvEncDestroyEncoder)(session);
                return Err(nv_err("注册 NV12 纹理", st));
            }

            // SPS/PPS（Annex-B）→ avcC。失败就留空，等第一个 IDR 帧自己解析
            let mut seq_buf = vec![0u8; 4096];
            let mut payload: NV_ENC_SEQUENCE_PARAM_PAYLOAD = std::mem::zeroed();
            payload.version = rung.sequence_param_payload_ver();
            payload.inBufferSize = seq_buf.len() as u32;
            payload.spsppsBuffer = seq_buf.as_mut_ptr() as *mut c_void;
            let mut out_size: u32 = 0;
            payload.outSPSPPSPayloadSize = &mut out_size;
            let mut extradata = None;
            if (api.nvEncGetSequenceParams)(session, &mut payload) == NVENCSTATUS::SUCCESS
                && out_size > 0
            {
                seq_buf.truncate(out_size as usize);
                extradata = crate::stream::flv::to_avcc_record(&seq_buf);
            }

            log::info!(
                "NVENC 已就绪：{}x{}@{}，{} kbps（{:?}），API {}.{}",
                cfg.width,
                cfg.height,
                cfg.fps,
                cfg.bitrate_kbps,
                cfg.rate_control,
                rung.major_minor().0,
                rung.major_minor().1
            );

            Ok(Self {
                api,
                rung,
                session,
                canvas,
                registered: reg.registeredResource,
                bitstream: bsb.bitstreamBuffer,
                width: cfg.width,
                height: cfg.height,
                gop,
                force_key: true,
                extradata,
                frame_idx: 0,
                conv: None,
                fps: cfg.fps,
            })
        }
    }

    pub fn extradata(&self) -> Option<Vec<u8>> {
        self.extradata.clone()
    }

    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    pub fn encode(&mut self, nv12: &[u8], _pts_100ns: i64) -> Result<Vec<EncodedVideo>, String> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_size = w * h;
        let need = y_size + w * (h / 2);
        if nv12.len() < need {
            return Err(format!("帧数据不足：需要 {need}，实际 {}", nv12.len()));
        }
        unsafe {
            // 1) 上传 NV12 到纹理
            self.canvas.context.UpdateSubresource(
                &self.canvas.texture,
                0,
                None,
                nv12.as_ptr() as *const c_void,
                self.width,
                0,
            );
            self.canvas.context.UpdateSubresource(
                &self.canvas.texture,
                1,
                None,
                nv12[y_size..].as_ptr() as *const c_void,
                self.width,
                0,
            );

            self.submit_frame()
        }
    }

    /// 硬件编码路径可以直接吃 BGRA 原始帧：色彩转换与缩放都在 GPU 上做，
    /// 省掉 CPU 侧 1080p 约 3.7 ms/帧的逐像素转换。
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        stride: usize,
        src_w: u32,
        src_h: u32,
    ) -> Result<Vec<EncodedVideo>, String> {
        let rebuild = match &self.conv {
            Some(c) => c.src_w != src_w || c.src_h != src_h,
            None => true,
        };
        if rebuild {
            self.conv = Some(super::d3d::BgraConverter::new(
                &self.canvas.device,
                &self.canvas.context,
                &self.canvas.texture,
                src_w,
                src_h,
                self.width,
                self.height,
                self.fps,
            )?);
        }
        if let Some(conv) = &self.conv {
            conv.convert(bgra, stride)?;
        }
        self.submit_frame()
    }

    /// 把已经落在 `canvas.texture` 的一帧交给 NVENC 并取回位流。
    fn submit_frame(&mut self) -> Result<Vec<EncodedVideo>, String> {
        unsafe {
            // 2) 映射已注册资源
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = self.rung.map_input_resource_ver();
            map.registeredResource = self.registered;
            let st = (self.api.nvEncMapInputResource)(self.session, &mut map);
            if st != NVENCSTATUS::SUCCESS {
                return Err(nv_err("映射输入纹理", st));
            }

            // 3) 编码
            let mut pic: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = self.rung.pic_params_ver();
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = self.width;
            pic.inputBuffer = map.mappedResource;
            pic.outputBitstream = self.bitstream;
            pic.bufferFmt = NV_ENC_BUFFER_FORMAT::NV12;
            pic.pictureStruct = NV_ENC_PIC_STRUCT::FRAME;
            pic.frameIdx = self.frame_idx;
            if self.force_key {
                // NV_ENC_PIC_FLAG_FORCEIDR
                pic.encodePicFlags = 0x1;
                self.force_key = false;
            }
            let st = (self.api.nvEncEncodePicture)(self.session, &mut pic);
            let _ = (self.api.nvEncUnmapInputResource)(self.session, map.mappedResource);
            if st != NVENCSTATUS::SUCCESS {
                return Err(nv_err("提交编码帧", st));
            }
            self.frame_idx = self.frame_idx.wrapping_add(1);

            // 4) 取位流
            let mut lock: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            lock.version = self.rung.lock_bitstream_ver();
            lock.outputBitstream = self.bitstream;
            let st = (self.api.nvEncLockBitstream)(self.session, &mut lock);
            if st != NVENCSTATUS::SUCCESS {
                return Err(nv_err("锁定位流", st));
            }
            let size = lock.bitstreamSizeInBytes as usize;
            let mut bytes = Vec::with_capacity(size);
            if size > 0 && !lock.bitstreamBufferPtr.is_null() {
                bytes.extend_from_slice(
                    std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, size),
                );
            }
            let keyframe = lock.pictureType == NV_ENC_PIC_TYPE::IDR
                || lock.pictureType == NV_ENC_PIC_TYPE::I;
            let _ = (self.api.nvEncUnlockBitstream)(self.session, self.bitstream);

            if bytes.is_empty() {
                return Ok(Vec::new());
            }

            // 首个 IDR 帧里也带着 SPS/PPS，如果之前没拿到就顺手解析
            if self.extradata.is_none() && keyframe {
                self.extradata = crate::stream::flv::to_avcc_record(&bytes);
            }

            let (avcc, _) = crate::stream::flv::annexb_to_avcc(&bytes);
            Ok(vec![EncodedVideo {
                data: avcc,
                keyframe,
            }])
        }
    }
}

impl Drop for NvencH264Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.registered.is_null() {
                let _ = (self.api.nvEncUnregisterResource)(self.session, self.registered);
            }
            if !self.bitstream.is_null() {
                let _ = (self.api.nvEncDestroyBitstreamBuffer)(self.session, self.bitstream);
            }
            if !self.session.is_null() {
                let _ = (self.api.nvEncDestroyEncoder)(self.session);
            }
        }
    }
}

/// 引擎侧统一的视频编码器：优先 NVENC，失败自动回退 Media Foundation。
pub enum VideoEncoder {
    Nvenc(NvencH264Encoder),
    Mf(MfH264Encoder),
}

impl VideoEncoder {
    pub fn new(cfg: &VideoConfig) -> Result<Self, String> {
        if cfg.prefer_nvenc {
            match NvencH264Encoder::new(cfg) {
                Ok(enc) => return Ok(Self::Nvenc(enc)),
                Err(e) => log::warn!("NVENC 不可用，回退 Media Foundation：{e}"),
            }
        }
        MfH264Encoder::new(cfg).map(Self::Mf)
    }

    pub fn name(&self) -> String {
        match self {
            Self::Nvenc(_) => "NVIDIA NVENC（硬件）".to_string(),
            Self::Mf(e) => format!(
                "{}（{}）",
                e.info.name,
                if e.info.hardware { "硬件" } else { "软件" }
            ),
        }
    }

    pub fn is_hardware(&self) -> bool {
        match self {
            Self::Nvenc(_) => true,
            Self::Mf(e) => e.info.hardware,
        }
    }

    /// 该编码器是否接受 BGRA 原始帧（自己完成 GPU 色彩转换）。
    pub fn prefers_bgra(&self) -> bool {
        matches!(self, Self::Nvenc(_))
    }

    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        stride: usize,
        src_w: u32,
        src_h: u32,
    ) -> Result<Vec<EncodedVideo>, String> {
        match self {
            Self::Nvenc(e) => e.encode_bgra(bgra, stride, src_w, src_h),
            Self::Mf(_) => Err("该编码器不接受 BGRA 输入".into()),
        }
    }

    pub fn extradata(&self) -> Option<Vec<u8>> {
        match self {
            Self::Nvenc(e) => e.extradata(),
            Self::Mf(e) => e.extradata(),
        }
    }

    pub fn request_keyframe(&mut self) {
        match self {
            Self::Nvenc(e) => e.request_keyframe(),
            Self::Mf(e) => e.request_keyframe(),
        }
    }

    pub fn encode(&mut self, nv12: &[u8], pts: i64) -> Result<Vec<EncodedVideo>, String> {
        match self {
            Self::Nvenc(e) => e.encode(nv12, pts),
            Self::Mf(e) => e.encode(nv12, pts),
        }
    }

    /// 停播时的收尾：MF 需要 DRAIN 把队列里的帧取出来；NVENC 是同步出帧，无需收尾。
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>, String> {
        match self {
            Self::Nvenc(_) => Ok(Vec::new()),
            Self::Mf(e) => e.finish(),
        }
    }
}
