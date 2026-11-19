//! Media Foundation H.264 硬件编码（NVENC / QSV / AMF 由系统 MFT 提供，软编 x264 兜底）。
//!
//! 只使用系统公开的 Media Foundation 转码器接口，不直接加载厂商私有 SDK，
//! 因此兼容性最好、体积最小。硬件 MFT 由系统在 MFTEnumEx 中自动暴露。

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFTEnumEx,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    MFT_REGISTER_TYPE_INFO, MF_VERSION, MFSTARTUP_LITE,
    MF_MT_MPEG2_PROFILE,
    MF_E_NOTACCEPTING, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_LOW_LATENCY,
};

use crate::config::{RateControl, VideoConfig};
use crate::stream::flv;

pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
}

pub struct EncodedVideo {
    /// AVCC（4 字节长度前缀）数据
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct MfH264Encoder {
    transform: IMFTransform,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// 硬件编码 MFT 通常是异步 MFT，需要事件驱动的输入节流
    async_mode: bool,
    event_gen: Option<windows::Win32::Media::MediaFoundation::IMFMediaEventGenerator>,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    fps: u32,
    frame_dur_100ns: i64,
    pub info: EncoderInfo,
    extradata: Option<Vec<u8>>,
    started: bool,
    force_key_next: bool,
    /// GPU 路径：有值时帧以 D3D11 纹理送编码器，否则走系统内存
    gpu: Option<super::d3d::GpuPath>,
}

unsafe impl Send for MfH264Encoder {}

fn ensure_mf() {
    unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    }
}

impl MfH264Encoder {
    pub fn new(cfg: &VideoConfig) -> Result<Self, String> {
        ensure_mf();
        let mut last_err = String::from("没有可用的 H.264 编码器");
        for (activate, info) in candidates(cfg)? {
            match Self::from_activate(activate, info, cfg) {
                Ok(enc) => {
                    log::info!("已选择编码器：{}（{}）", enc.info.name,
                        if enc.info.hardware { "硬件" } else { "软件" });
                    return Ok(enc);
                }
                Err(e) => {
                    log::warn!("编码器候选不可用：{e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    fn from_activate(
        activate: IMFActivate,
        info: EncoderInfo,
        cfg: &VideoConfig,
    ) -> Result<Self, String> {
        let transform: IMFTransform = unsafe { activate.ActivateObject() }
            .map_err(|e| format!("{} 激活失败: {e}", info.name))?;
        unsafe {
            // 硬件编码器（NVENC/QSV/AMF）通常标记为异步 MFT：
            // 必须先用 MF_TRANSFORM_ASYNC_UNLOCK 解锁，否则 SetInputType 会返回
            // MF_E_TRANSFORM_ASYNC_LOCKED (0xC00D6D77)。
            let attrs = transform.GetAttributes().ok();
            let async_mode = attrs
                .as_ref()
                .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok())
                .unwrap_or(0)
                != 0;
            // 直播必须关掉编码器内部缓冲（否则要等 DRAIN 才出帧，延迟会累积到无法直播）
            if let Some(a) = attrs.as_ref() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            // 直播关键约束：必须 baseline / 无 B 帧。
            // 多数直播服务端（含 B 站、腾讯云）遇到含 B 帧或高 profile 的流会“推一秒就断开”。
            configure_low_latency_codec(&transform, cfg);
            let event_gen = if async_mode {
                if let Some(a) = attrs.as_ref() {
                    let _ = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                }
                transform
                    .cast::<windows::Win32::Media::MediaFoundation::IMFMediaEventGenerator>()
                    .ok()
            } else {
                None
            };

            // 输入类型：NV12
            let in_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("创建输入类型失败: {e}"))?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("设置输入主类型失败: {e}"))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| format!("设置输入子类型失败: {e}"))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| format!("设置逐行扫描失败: {e}"))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, pack64(cfg.width, cfg.height))
                .map_err(|e| format!("设置帧尺寸失败: {e}"))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack64(cfg.fps, 1))
                .map_err(|e| format!("设置帧率失败: {e}"))?;
            in_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack64(1, 1))
                .map_err(|e| format!("设置像素宽高比失败: {e}"))?;

            // 输出类型：H.264
            let out_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("创建输出类型失败: {e}"))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("设置输出主类型失败: {e}"))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| format!("设置输出子类型失败: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000))
                .map_err(|e| format!("设置码率失败: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| format!("设置输出逐行失败: {e}"))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, pack64(cfg.width, cfg.height))
                .map_err(|e| format!("设置输出帧尺寸失败: {e}"))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack64(cfg.fps, 1))
                .map_err(|e| format!("设置输出帧率失败: {e}"))?;
            out_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack64(1, 1))
                .map_err(|e| format!("设置输出像素比失败: {e}"))?;
            // GOP：MF_MT_MAX_KEYFRAME_SPACING 控制关键帧间隔
            let gop = cfg.gop_seconds.saturating_mul(cfg.fps).max(1);
            out_type
                .SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, gop)
                .map_err(|e| format!("设置 GOP 失败: {e}"))?;
            // 强制 Baseline profile（66）：不含 B 帧、无 CABAC，兼容性最好
            let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 66);

            // CBR 由 avg bitrate 与编码器默认行为共同决定；VBR 时下调目标码率留足峰值空间
            if cfg.rate_control == RateControl::Vbr {
                let vbr = (cfg.bitrate_kbps as f64 * 0.8) as u32;
                let _ = out_type.SetUINT32(&MF_MT_AVG_BITRATE, vbr.saturating_mul(1000));
            }

            // 硬件编码器只吃 D3D11 纹理。
            // 注意：部分硬件 MFT（NVENC / QSV）在“已设好类型”之后才收到 D3D 设备管理器会崩溃，
            // 因此这里在设置任何类型之前就把设备管理器交给它（微软文档的推荐顺序）。
            let mut gpu = None;
            let gpu_requested =
                info.hardware && cfg.prefer_gpu && std::env::var_os("LITE_GPU").is_some();
            if gpu_requested {
                let hint = if info.name.to_ascii_lowercase().contains("intel")
                    || info.name.to_ascii_lowercase().contains("quick sync")
                {
                    "intel"
                } else if info.name.to_ascii_lowercase().contains("nvidia")
                    || info.name.to_ascii_lowercase().contains("nvenc")
                {
                    "nvidia"
                } else if info.name.to_ascii_lowercase().contains("amd")
                    || info.name.to_ascii_lowercase().contains("amf")
                {
                    "amd"
                } else {
                    ""
                };
                match super::d3d::GpuPath::new(cfg.width, cfg.height, hint) {
                    Ok(path) => {
                        transform
                            .ProcessMessage(
                                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_SET_D3D_MANAGER,
                                path.token as usize,
                            )
                            .map_err(|e| format!("交给编码器 D3D 设备管理器失败: {e}"))?;
                        log::info!("已启用 GPU 纹理直通编码（适配器提示：{hint}）");
                        gpu = Some(path);
                    }
                    Err(e) => {
                        log::warn!("{} 的 D3D11 路径不可用: {e}", info.name);
                    }
                }
            }

            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("编码器不接受 H.264 输出: {e}"))?;

            // 顺序很关键：MF 编码器要求先设输出类型，再设输入类型
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("编码器不接受 NV12 输入: {e}"))?;


            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("读取输出流信息失败: {e}"))?;

            // 序列头（AVCDecoderConfigurationRecord）：MF 给的往往是 Annex-B，必须规整
            let mut extradata = None;
            if let Ok(blob) = get_blob(&out_type, &MF_MT_MPEG_SEQUENCE_HEADER) {
                extradata = flv::to_avcc_record(&blob);
            }

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("编码器启动失败: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("编码器启动失败: {e}"))?;

            let mut enc = Self {
                transform,
                out_info,
                async_mode,
                event_gen,
                width: cfg.width & !1,
                height: cfg.height & !1,
                fps: cfg.fps.max(1),
                frame_dur_100ns: (10_000_000i64 / cfg.fps.max(1) as i64).max(1),
                info,
                extradata,
                started: true,
                force_key_next: false,
                gpu,
            };
            // 预热验证：部分硬件 MFT 虽接受 NV12 类型，但实际只支持 D3D11 纹理输入，
            // 送系统内存帧会被拒绝或永远不产出。这里真跑几帧，产不出码流就换下一个候选。
            enc.warmup().map_err(|e| format!("{} 预热失败: {e}", enc.info.name))?;
            Ok(enc)
        }
    }

    fn warmup(&mut self) -> Result<(), String> {
        let mut black = Vec::new();
        crate::convert::black_nv12(self.width, self.height, &mut black);
        // 硬件编码器（NVENC/QSV/AMF）内部有流水线延迟，往往要 8~12 帧才吐第一帧，
        // 之前固定 6 帧会把可用的硬件编码器误判成“不产出”而淘汰，导致回退到软件编码。
        let attempts: i64 = if self.info.hardware { 14 } else { 6 };
        let mut produced = 0usize;
        for i in 0..attempts {
            match self.encode(&black, i * self.frame_dur_100ns) {
                Ok(frames) => produced += frames.len(),
                Err(e) => return Err(e),
            }
        }
        if produced == 0 {
            return Err(format!(
                "预热 {attempts} 帧未产出任何码流（编码器不接受系统内存帧）"
            ));
        }
        Ok(())
    }

    pub fn extradata(&self) -> Option<Vec<u8>> {
        self.extradata.clone()
    }

    pub fn request_keyframe(&mut self) {
        self.force_key_next = true;
    }

    /// 编码一帧 NV12。返回本帧产出的所有 H.264 帧（通常 1 帧，B 帧关闭时为 1）。
    pub fn encode(&mut self, nv12: &[u8], pts_100ns: i64) -> Result<Vec<EncodedVideo>, String> {
        unsafe {
            let sample: IMFSample = match self.gpu.as_ref() {
                // GPU 路径：NV12 上传到 D3D11 纹理，编码器直接读纹理
                Some(path) => path.make_sample(nv12)?,
                None => {
                    let size = nv12.len();
                    let sample: IMFSample =
                        MFCreateSample().map_err(|e| format!("创建采样失败: {e}"))?;
                    let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(size as u32)
                        .map_err(|e| format!("创建缓冲区失败: {e}"))?;
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len = 0u32;
                    buffer
                        .Lock(&mut ptr, Some(&mut max_len), None)
                        .map_err(|e| format!("锁定缓冲区失败: {e}"))?;
                    std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, size.min(max_len as usize));
                    buffer
                        .Unlock()
                        .map_err(|e| format!("解锁缓冲区失败: {e}"))?;
                    let _ = buffer.SetCurrentLength(size as u32);
                    sample
                        .AddBuffer(&buffer)
                        .map_err(|e| format!("挂载缓冲区失败: {e}"))?;
                    sample
                }
            };
            sample
                .SetSampleTime(pts_100ns)
                .map_err(|e| format!("设置时间戳失败: {e}"))?;
            sample
                .SetSampleDuration(self.frame_dur_100ns)
                .map_err(|e| format!("设置时长失败: {e}"))?;

            if self.force_key_next {
                // 清除 clean point 让编码器按 GOP 输出 IDR
                let _ = sample.SetUINT32(
                    &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                    1,
                );
                self.force_key_next = false;
            }

            let mut out: Vec<EncodedVideo> = Vec::new();
            // 异步 MFT 必须先等到 METransformNeedInput 再送帧，否则会 E_UNEXPECTED
            if self.async_mode {
                self.pump_events(&mut out)?;
            }
            let mut guard = 0u32;
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
            Ok(()) => break,
                    Err(e) => {
                        // 异步 MFT 在缓冲区满时会返回 MF_E_NOTACCEPTING，
                        // 此时需要先消费事件队列（可能产出输出帧），再重试。
                        if e.code() == MF_E_NOTACCEPTING && self.async_mode {
                            guard += 1;
                            if guard > 4 {
                                return Err("编码器持续拒绝输入".to_string());
                            }
                            self.pump_events(&mut out)?;
                            continue;
                        }
                        return Err(format!("提交编码帧失败: {e}"));
                    }
                }
            }

            out.extend(self.drain_outputs()?);
            Ok(out)
        }
    }

    /// 消费异步 MFT 的事件队列，直到收到“需要输入”为止（带超时，避免死等）。
    fn pump_events(&mut self, out: &mut Vec<EncodedVideo>) -> Result<(), String> {
        use windows::Win32::Media::MediaFoundation::{
            METransformHaveOutput, METransformNeedInput, MF_EVENT_FLAG_NO_WAIT,
        };
        let Some(gen) = self.event_gen.clone() else {
            std::thread::sleep(std::time::Duration::from_millis(2));
            return Ok(());
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(30);
        loop {
            unsafe {
                match gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(ev) => {
                        let kind = match ev.GetType() {
                            Ok(k) => k,
                            Err(_) => continue,
                        };
                        if kind == METransformNeedInput.0 as u32 {
                            return Ok(());
                        }
                        if kind == METransformHaveOutput.0 as u32 {
                            out.extend(self.drain_outputs()?);
                        }
                    }
                    Err(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Ok(());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
        }
    }

    fn drain_outputs(&mut self) -> Result<Vec<EncodedVideo>, String> {
        let mut out = Vec::new();
        if self.async_mode {
            // 异步 MFT：只有收到 METransformHaveOutput 才允许调用 ProcessOutput，
            // 否则 ProcessOutput 会返回 E_UNEXPECTED。
            use windows::Win32::Media::MediaFoundation::{
                METransformHaveOutput, METransformNeedInput, MF_EVENT_FLAG_NO_WAIT,
            };
            if let Some(gen) = self.event_gen.clone() {
                let mut guard = 0u32;
                loop {
                    guard += 1;
                    if guard > 16 {
                        break;
                    }
                    unsafe {
                        match gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                            Ok(ev) => {
                                let kind = match ev.GetType() {
                                    Ok(k) => k,
                                    Err(_) => break,
                                };
                                if kind == METransformHaveOutput.0 as u32 {
                                    if let Some(f) = self.pull_one()? {
                                        out.push(f);
                                    }
                                } else if kind == METransformNeedInput.0 as u32 {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            return Ok(out);
        }

        loop {
            match self.pull_one()? {
                Some(f) => out.push(f),
                None => break,
            }
        }
        Ok(out)
    }

    /// 取出一个输出帧：无输出时返回 None；真错误才返回 Err。
    fn pull_one(&mut self) -> Result<Option<EncodedVideo>, String> {
        unsafe {
            let mut data_buffer = MFT_OUTPUT_DATA_BUFFER::default();
            data_buffer.dwStreamID = 0;
            // 如果 MFT 自己提供采样，pSample 必须为空；否则由我们提供
            let provides = self.out_info.dwFlags
                & (windows::Win32::Media::MediaFoundation::MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                    as u32);
            if provides == 0 {
                let buf_size = self.out_info.cbSize.max(256 * 1024);
                let sample: IMFSample =
                    MFCreateSample().map_err(|e| format!("创建输出采样失败: {e}"))?;
                let buffer: IMFMediaBuffer =
                    MFCreateMemoryBuffer(buf_size).map_err(|e| format!("创建输出缓冲失败: {e}"))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| format!("挂载输出缓冲失败: {e}"))?;
                data_buffer.pSample = std::mem::ManuallyDrop::new(Some(sample));
            }

            let mut status = 0u32;
            let mut bufs = [data_buffer];
            let res = self.transform.ProcessOutput(0, &mut bufs, &mut status);
            match res {
                Ok(()) => {
                    let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
                    let events = std::mem::ManuallyDrop::take(&mut bufs[0].pEvents);
                    drop(events);
                    if let Some(sample) = sample {
                        return Ok(self.sample_to_frame(&sample));
                    }
                    Ok(None)
                }
                Err(e) => {
                    if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                        return Ok(None);
                    }
                    if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                        // 编码器会在首帧后更新输出类型（带真实 SPS/PPS 与 profile），
                        // 必须重新协商，否则后续取输出会一直失败。
                        self.renegotiate_output()?;
                        return Ok(None);
                    }
                    Err(format!("读取编码输出失败: {e}"))
                }
            }
        }
    }

    /// 处理 MF_E_TRANSFORM_STREAM_CHANGE：重新选择并设置输出类型，同时刷新序列头。
    fn renegotiate_output(&mut self) -> Result<(), String> {
        unsafe {
            let mut chosen: Option<IMFMediaType> = None;
            let mut best = u32::MAX;
            for i in 0..16u32 {
                let Ok(t) = self.transform.GetOutputAvailableType(0, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_MAJOR_TYPE).map(|g| g == MFMediaType_Video).unwrap_or(false)
                    && t.GetGUID(&MF_MT_SUBTYPE)
                        .map(|g| g == MFVideoFormat_H264)
                        .unwrap_or(false)
                {
                    // 偏好带序列头的类型
                    let has_hdr = get_blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER)
                        .map(|b| b.len() > 4)
                        .unwrap_or(false);
                    let score = if has_hdr { 0 } else { 1 };
                    if score < best {
                        best = score;
                        chosen = Some(t);
                    }
                    if has_hdr {
                        break;
                    }
                }
            }
            let Some(t) = chosen else {
                return Err("编码器更新输出类型后没有可用的 H.264 类型".to_string());
            };
            self.transform
                .SetOutputType(0, &t, 0)
                .map_err(|e| format!("重设输出类型失败: {e}"))?;
            if let Ok(blob) = get_blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER) {
                if self.extradata.is_none() {
                    self.extradata = flv::to_avcc_record(&blob);
                }
            }
            self.out_info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("刷新输出信息失败: {e}"))?;
            Ok(())
        }
    }

    fn sample_to_frame(&mut self, sample: &IMFSample) -> Option<EncodedVideo> {
        unsafe {
            let buffer: IMFMediaBuffer = sample.GetBufferByIndex(0).ok()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
            let raw = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(0);
            self.transform_annexb_to_avcc(raw)
        }
    }

    fn transform_annexb_to_avcc(&mut self, raw: Vec<u8>) -> Option<EncodedVideo> {
        let (data, was_annexb) = flv::annexb_to_avcc(&raw);
        let keyframe = flv::split_nalus(&raw)
            .iter()
            .any(|n| flv::nal_unit_type(n) == 5);

        // 兜底：若编码器没给 avcC，就用首帧里的 SPS/PPS 自建
        if self.extradata.is_none() && was_annexb {
            let nalus = flv::split_nalus(&raw);
            let sps = nalus.iter().find(|n| flv::nal_unit_type(n) == 7);
            let pps = nalus.iter().find(|n| flv::nal_unit_type(n) == 8);
            if let (Some(sps), Some(pps)) = (sps, pps) {
                self.extradata = Some(flv::build_avcc(sps, pps));
            }
        }

        Some(EncodedVideo { data, keyframe })
    }

    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>, String> {
        if !self.started {
            return Ok(Vec::new());
        }
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        let frames = self.drain_outputs()?;
        self.started = false;
        Ok(frames)
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

const MF_E_TRANSFORM_NEED_MORE_INPUT: windows::core::HRESULT =
    windows::core::HRESULT(0xC00D6D72u32 as i32);
const MF_E_TRANSFORM_STREAM_CHANGE: windows::core::HRESULT =
    windows::core::HRESULT(0xC00D6D71u32 as i32);

/// 列出本机所有可枚举的 H.264 编码器（名称 + 是否硬件），用于诊断。
pub fn describe_candidates() -> Vec<String> {
    ensure_mf();
    match candidates(&VideoConfig::default()) {
        Ok(list) => list
            .iter()
            .map(|(_, i)| format!("{}（{}）", i.name, if i.hardware { "硬件" } else { "软件" }))
            .collect(),
        Err(e) => vec![e],
    }
}

/// 通过 ICodecAPI 强制“零 B 帧 + CBR + 固定 GOP”，满足直播服务端要求。
fn configure_low_latency_codec(transform: &IMFTransform, cfg: &VideoConfig) {
    use windows::core::VARIANT;
    use windows::Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
        CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
        CODECAPI_AVEncMPVGOPSize, ICodecAPI,
    };
    use crate::config::RateControl;

    unsafe {
        let Ok(codec) = transform.cast::<ICodecAPI>() else {
            // 极少数 MFT 不实现 ICodecAPI，此时只能靠输出类型里的 avg bitrate
            log::warn!("编码器不支持 ICodecAPI，码率控制退化为输出类型参数");
            return;
        };
        let set = |guid: &windows::core::GUID, value: u32| {
            let v = VARIANT::from(value);
            let _ = codec.SetValue(guid, &v);
        };
        // 目标码率（bps）。**这是关键**：多数 MFT（尤其硬件编码器）只认
        // CODECAPI_AVEncCommonMeanBitRate，忽略输出类型里的 MF_MT_AVG_BITRATE，
        // 不设置就会出现“设 8000 实际只跑 2400”的现象。
        let target_bps = cfg.bitrate_kbps.saturating_mul(1000);
        // 0 = CBR，1 = PeakConstrainedVBR(ABR)，2 = UnconstrainedVBR
        let mode = match cfg.rate_control {
            RateControl::Cbr => 0u32,
            RateControl::Abr => 1u32,
            RateControl::Vbr => 2u32,
        };
        set(&CODECAPI_AVEncCommonRateControlMode, mode);
        match cfg.rate_control {
            RateControl::Cbr => {
                set(&CODECAPI_AVEncCommonMeanBitRate, target_bps);
                set(&CODECAPI_AVEncCommonMaxBitRate, target_bps);
            }
            RateControl::Abr => {
                // 均值 = 目标，峰值 = 2 倍（画面剧烈变化时临时上冲，静止回落）
                set(&CODECAPI_AVEncCommonMeanBitRate, target_bps);
                set(&CODECAPI_AVEncCommonMaxBitRate, target_bps.saturating_mul(2));
            }
            RateControl::Vbr => {
                set(&CODECAPI_AVEncCommonMeanBitRate, (target_bps as f64 * 0.8) as u32);
                set(&CODECAPI_AVEncCommonMaxBitRate, target_bps.saturating_mul(3));
            }
        }
        // 0 个 B 帧（绝对不能有，否则多数直播服务端直接踢流）
        set(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        let gop = cfg.gop_seconds.saturating_mul(cfg.fps).max(1);
        set(&CODECAPI_AVEncMPVGOPSize, gop);
        log::info!(
            "码率控制：{:?}，目标 {} kbps，模式值 {}",
            cfg.rate_control,
            cfg.bitrate_kbps,
            mode
        );
    }
}

fn pack64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// 从属性读 blob（用于序列头）。
fn get_blob(attrs: &IMFMediaType, key: &windows::core::GUID) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let size = attrs.GetBlobSize(key)?;
        let mut buf = vec![0u8; size as usize];
        let mut written = 0u32;
        attrs.GetBlob(key, &mut buf, Some(&mut written))?;
        buf.truncate(written as usize);
        Ok(buf)
    }
}

/// 枚举系统中所有 H.264 编码 MFT，硬件优先、软件兜底。
/// 返回全部候选，由调用方逐个尝试配置，避免某一个厂商编码器不可用就整体失败。
fn candidates(cfg: &VideoConfig) -> Result<Vec<(IMFActivate, EncoderInfo)>, String> {
    unsafe {
        let in_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let out_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_ALL,
            Some(&in_type),
            Some(&out_type),
            &mut activates,
            &mut count,
        )
        .map_err(|e| format!("枚举视频编码器失败: {e}"))?;
        if count == 0 || activates.is_null() {
            return Err("系统没有可用的 H.264 编码器 MFT".into());
        }
        let list = std::slice::from_raw_parts(activates, count as usize);

        let mut hw: Vec<(IMFActivate, EncoderInfo)> = Vec::new();
        let mut sw: Vec<(IMFActivate, EncoderInfo)> = Vec::new();
        for slot in list {
            let Some(activate) = slot else { continue };
            let name = friendly_name(activate);
            let hardware = is_hardware(activate);
            let info = EncoderInfo { name, hardware };
            if hardware {
                hw.push((activate.clone(), info));
            } else {
                sw.push((activate.clone(), info));
            }
        }
        co_task_free(activates);
        if hw.is_empty() && sw.is_empty() {
            return Err("无法激活任何 H.264 编码器".to_string());
        }
        log::info!(
            "检测到 {} 个 H.264 编码器：硬件 {} 个（{}），软件 {} 个",
            hw.len() + sw.len(),
            hw.len(),
            hw.iter()
                .map(|(_, i)| i.name.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
            sw.len()
        );
        hw.extend(sw);
        // 未开启 GPU 加速时，硬件编码器必然因为只吃 D3D11 纹理而被拒，
        // 直接排除它们，省掉一轮无用的激活与预热开销。
        if !cfg.prefer_gpu {
            hw.retain(|(_, i)| !i.hardware);
            log::info!("GPU 硬件编码未开启，仅使用软件编码器");
            return Ok(hw);
        }

        // 硬件候选按“独显优先”排序：NVENC → QSV → AMF。
        // 部分机器上 QSV 的 MF 封装会在收到 D3D11 设备管理器时崩溃，
        // 优先试 NVENC 可以避免踩到它；软件编码永远排在最后兜底。
        let rank = |name: &str| -> u8 {
            let n = name.to_ascii_lowercase();
            if n.contains("nvidia") || n.contains("nvenc") {
                0
            } else if n.contains("intel") || n.contains("quick sync") {
                1
            } else if n.contains("amd") || n.contains("amf") {
                2
            } else {
                3
            }
        };
        let mut hardware_part = Vec::new();
        let mut software_part = Vec::new();
        for item in hw {
            if item.1.hardware {
                hardware_part.push(item);
            } else {
                software_part.push(item);
            }
        }
        hardware_part.sort_by_key(|(_, i)| rank(&i.name));
        hardware_part.extend(software_part);
        Ok(hardware_part)
    }
}

unsafe fn co_task_free(ptr: *mut Option<IMFActivate>) {
    let _ = ptr;
    // MFTEnumEx 分配的数组由 CoTaskMemFree 释放
    windows::Win32::System::Com::CoTaskMemFree(Some(ptr as *const std::ffi::c_void));
}

unsafe fn friendly_name(activate: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    let key = windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    if activate.GetString(&key, &mut buf[..], Some(&mut len)).is_ok() && len > 0 {
        let n = (len as usize).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n.saturating_sub(1)]);
        if !s.trim().is_empty() {
            return s;
        }
    }
    "H.264 MFT".to_string()
}

unsafe fn is_hardware(activate: &IMFActivate) -> bool {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    let key = windows::Win32::Media::MediaFoundation::MFT_ENUM_HARDWARE_URL_Attribute;
    if activate.GetString(&key, &mut buf[..], Some(&mut len)).is_ok() && len > 1 {
        return true;
    }
    let name = friendly_name(activate).to_lowercase();
    ["nvidia", "nvenc", "intel", "quick sync", "qsv", "amd", "amf", "hardware"]
        .iter()
        .any(|k| name.contains(k))
}
