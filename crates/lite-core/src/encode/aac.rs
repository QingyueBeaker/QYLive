//! Media Foundation AAC-LC 编码（48kHz / 立体声 / 1024 采样一帧），推流侧用 FLV 裸 AAC。

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFStartup, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFTEnumEx, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_ALL,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_STREAM_INFO,
    MFT_REGISTER_TYPE_INFO, MF_VERSION, MFSTARTUP_LITE,
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_SAMPLES: usize = 1024;

pub struct MfAacEncoder {
    transform: IMFTransform,
    out_info: MFT_OUTPUT_STREAM_INFO,
    ts_100ns: i64,
    frame_dur: i64,
}

unsafe impl Send for MfAacEncoder {}

const MF_E_TRANSFORM_NEED_MORE_INPUT: windows::core::HRESULT =
    windows::core::HRESULT(0xC00D6D72u32 as i32);

impl MfAacEncoder {
    pub fn new(bitrate_kbps: u32) -> Result<Self, String> {
        unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
            let (transform, _name) = pick_encoder()?;

            let in_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("创建 AAC 输入类型失败: {e}"))?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| e.to_string())?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * 4)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .map_err(|e| e.to_string())?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("AAC 编码器不接受 PCM 输入: {e}"))?;

            let out_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("创建 AAC 输出类型失败: {e}"))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| e.to_string())?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(
                    &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    bitrate_kbps.saturating_mul(1000) / 8,
                )
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                .map_err(|e| e.to_string())?;
            // 0x29 = AAC-LC, 采样率 <= 48kHz
            out_type
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                .map_err(|e| e.to_string())?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("AAC 编码器输出类型设置失败: {e}"))?;

            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("读取 AAC 输出信息失败: {e}"))?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| e.to_string())?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| e.to_string())?;

            Ok(Self {
                transform,
                out_info,
                ts_100ns: 0,
                frame_dur: (FRAME_SAMPLES as i64 * 10_000_000) / SAMPLE_RATE as i64,
            })
        }
    }

    /// AudioSpecificConfig：AAC-LC / 48kHz / 立体声 -> 0x12 0x10
    pub fn asc(&self) -> Vec<u8> {
        vec![0x12, 0x10]
    }

    /// 编码一帧 1024 采样立体声 i16。
    pub fn encode(&mut self, samples: &[i16]) -> Result<Vec<Vec<u8>>, String> {
        unsafe {
            let byte_len = samples.len() * 2;
            let sample: IMFSample = MFCreateSample().map_err(|e| e.to_string())?;
            let buffer: IMFMediaBuffer =
                MFCreateMemoryBuffer(byte_len as u32).map_err(|e| e.to_string())?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .map_err(|e| e.to_string())?;
            let dst = std::slice::from_raw_parts_mut(ptr, max_len as usize);
            let src = std::slice::from_raw_parts(samples.as_ptr() as *const u8, byte_len);
            let n = byte_len.min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
            buffer.Unlock().map_err(|e| e.to_string())?;
            let _ = buffer.SetCurrentLength(byte_len as u32);
            sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
            sample
                .SetSampleTime(self.ts_100ns)
                .map_err(|e| e.to_string())?;
            sample
                .SetSampleDuration(self.frame_dur)
                .map_err(|e| e.to_string())?;
            self.ts_100ns += self.frame_dur;

            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| format!("提交音频帧失败: {e}"))?;
            self.drain()
        }
    }

    fn drain(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        unsafe {
            loop {
                let mut data_buffer =
                    windows::Win32::Media::MediaFoundation::MFT_OUTPUT_DATA_BUFFER::default();
                let provides = self.out_info.dwFlags
                    & (windows::Win32::Media::MediaFoundation::MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        as u32);
                if provides == 0 {
                    let sample: IMFSample = MFCreateSample().map_err(|e| e.to_string())?;
                    let buffer: IMFMediaBuffer =
                        MFCreateMemoryBuffer(self.out_info.cbSize.max(16384))
                            .map_err(|e| e.to_string())?;
                    sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
                    data_buffer.pSample = std::mem::ManuallyDrop::new(Some(sample));
                }
                let mut status = 0u32;
                let mut bufs = [data_buffer];
                match self.transform.ProcessOutput(0, &mut bufs, &mut status) {
                    Ok(()) => {
                        let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
                        if let Some(sample) = sample {
                            if let Ok(Some(bytes)) = read_sample(&sample) {
                                if !bytes.is_empty() {
                                    out.push(bytes);
                                }
                            }
                        }
                        let events = std::mem::ManuallyDrop::take(&mut bufs[0].pEvents);
                        drop(events);
                    }
                    Err(e) => {
                        if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                            break;
                        }
                        return Err(format!("读取 AAC 输出失败: {e}"));
                    }
                }
            }
        }
        Ok(out)
    }
}

fn read_sample(sample: &IMFSample) -> windows::core::Result<Option<Vec<u8>>> {
    unsafe {
        let buffer: IMFMediaBuffer = sample.GetBufferByIndex(0)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        let _ = buffer.SetCurrentLength(0);
        Ok(Some(data))
    }
}

fn pick_encoder() -> Result<(IMFTransform, String), String> {
    unsafe {
        let in_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_PCM,
        };
        let out_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_ALL,
            Some(&in_type),
            Some(&out_type),
            &mut activates,
            &mut count,
        )
        .map_err(|e| format!("枚举 AAC 编码器失败: {e}"))?;
        let list = std::slice::from_raw_parts(activates, count as usize);
        for slot in list {
            if let Some(activate) = slot {
                if let Ok(transform) = activate.ActivateObject::<IMFTransform>() {
                    return Ok((transform, "MF AAC".into()));
                }
            }
        }
        Err("系统没有可用的 AAC 编码器".into())
    }
}
