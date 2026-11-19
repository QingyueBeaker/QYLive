//! FLV 封装：把编码器输出的裸流打包成 FLV tag，供 RTMP 直接发送。
//!
//! 一个 tag = 11 字节 tag header + 数据，RTMP 的 message payload 即整段 tag。
//! H.264 走 AVC 视频包（codec id 7，payload 为 AVCC / AVCDecoderConfigurationRecord），
//! AAC 走 AAC 音频包（sound format 10，payload 为 ASC 或裸帧）。

pub const TAG_AUDIO: u8 = 8;
pub const TAG_VIDEO: u8 = 9;
pub const TAG_DATA: u8 = 18;

/// 一条待发送的 FLV tag。
pub struct FlvTag {
    pub tag_type: u8,
    /// 毫秒时间戳
    pub ts_ms: u32,
    pub payload: Vec<u8>,
}

impl FlvTag {
    /// 序列化为完整 tag（含 tag header 与 PreviousTagSize 之前的 tag 头）。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(11 + self.payload.len());
        out.push(self.tag_type);
        out.extend_from_slice(&((self.payload.len() as u32) & 0x00FF_FFFF).to_be_bytes()[1..]);
        out.extend_from_slice(&((self.ts_ms & 0x00FF_FFFF) as u32).to_be_bytes()[1..]);
        out.push(((self.ts_ms >> 24) & 0xFF) as u8);
        out.extend_from_slice(&[0, 0, 0]); // stream id 恒为 0
        out.extend_from_slice(&self.payload);
        out.extend_from_slice(&((11 + self.payload.len()) as u32).to_be_bytes());
        out
    }
}

/// H.264 sequence header（AVCDecoderConfigurationRecord）。
pub fn video_sequence_header(avcc: &[u8]) -> FlvTag {
    let mut payload = Vec::with_capacity(5 + avcc.len());
    payload.push(0x17); // key frame + AVC
    payload.push(0x00); // AVC sequence header
    payload.extend_from_slice(&[0, 0, 0]); // composition time = 0
    payload.extend_from_slice(avcc);
    FlvTag {
        tag_type: TAG_VIDEO,
        ts_ms: 0,
        payload,
    }
}

/// H.264 帧（payload 必须已是 4 字节长度前缀的 AVCC 格式）。
pub fn video_frame(payload_bytes: Vec<u8>, ts_ms: u32, keyframe: bool) -> FlvTag {
    let mut payload = Vec::with_capacity(5 + payload_bytes.len());
    payload.push(if keyframe { 0x17 } else { 0x27 });
    payload.push(0x01); // AVC NALU
    payload.extend_from_slice(&[0, 0, 0]); // composition time = 0
    payload.extend_from_slice(&payload_bytes);
    FlvTag {
        tag_type: TAG_VIDEO,
        ts_ms,
        payload,
    }
}

/// AAC sequence header（AudioSpecificConfig）。
pub fn audio_sequence_header(asc: &[u8]) -> FlvTag {
    let mut payload = Vec::with_capacity(2 + asc.len());
    payload.push(0xAF); // AAC / 44k+ / 16bit / stereo
    payload.push(0x00); // AAC sequence header
    payload.extend_from_slice(asc);
    FlvTag {
        tag_type: TAG_AUDIO,
        ts_ms: 0,
        payload,
    }
}

/// AAC 裸帧。
pub fn audio_frame(payload_bytes: Vec<u8>, ts_ms: u32) -> FlvTag {
    let mut payload = Vec::with_capacity(2 + payload_bytes.len());
    payload.push(0xAF);
    payload.push(0x01); // AAC raw
    payload.extend_from_slice(&payload_bytes);
    FlvTag {
        tag_type: TAG_AUDIO,
        ts_ms,
        payload,
    }
}

/// onMetaData（使用 AMF0 Object，兼容主流服务端）。
pub fn metadata(width: u32, height: u32, fps: u32, bitrate_kbps: u32, audio_kbps: u32) -> FlvTag {
    let mut payload = Vec::new();
    crate::stream::amf::encode(
        &[
            crate::stream::amf::s("@setDataFrame"),
            crate::stream::amf::s("onMetaData"),
            crate::stream::amf::Amf0::Object(vec![
                ("duration".into(), crate::stream::amf::n(0.0)),
                ("width".into(), crate::stream::amf::n(width as f64)),
                ("height".into(), crate::stream::amf::n(height as f64)),
                ("framerate".into(), crate::stream::amf::n(fps as f64)),
                ("videocodecid".into(), crate::stream::amf::n(7.0)),
                ("audiocodecid".into(), crate::stream::amf::n(10.0)),
                (
                    "videodatarate".into(),
                    crate::stream::amf::n(bitrate_kbps as f64),
                ),
                (
                    "audiodatarate".into(),
                    crate::stream::amf::n(audio_kbps as f64),
                ),
                ("encoder".into(), crate::stream::amf::s("LiteRTMP")),
            ]),
        ],
        &mut payload,
    );
    FlvTag {
        tag_type: TAG_DATA,
        ts_ms: 0,
        payload,
    }
}

/// 把 Annex-B（00 00 00 01 / 00 00 01 起始码）转成 4 字节长度前缀的 AVCC 形式。
/// 如果输入已经是长度前缀形式，则原样返回。
pub fn annexb_to_avcc(data: &[u8]) -> (Vec<u8>, bool) {
    if !data.starts_with(&[0, 0, 0, 1]) && !data.starts_with(&[0, 0, 1]) {
        return (data.to_vec(), false);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut nal_start: Option<usize> = None;
    let mut i = 0usize;
    while i + 3 < data.len() {
        let start4 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        let start3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if start4 || start3 {
            if let Some(s) = nal_start {
                let end = i;
                if end > s {
                    out.extend_from_slice(&((end - s) as u32).to_be_bytes());
                    out.extend_from_slice(&data[s..end]);
                }
            }
            i += if start4 { 4 } else { 3 };
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        if s < data.len() {
            out.extend_from_slice(&((data.len() - s) as u32).to_be_bytes());
            out.extend_from_slice(&data[s..]);
        }
    }
    (out, true)
}

/// 从 Annex-B 数据中提取 NALU（用于兜底构造 avcC）。
pub fn split_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i + 3 < data.len() {
        let start4 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        let start3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if start4 || start3 {
            if let Some(s) = start {
                if i > s {
                    out.push(&data[s..i]);
                }
            }
            i += if start4 { 4 } else { 3 };
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

/// 用 SPS/PPS 构造 AVCDecoderConfigurationRecord（当编码器未提供 extradata 时兜底）。
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = vec![
        0x01, // configurationVersion
        sps.get(1).copied().unwrap_or(0x42),
        sps.get(2).copied().unwrap_or(0x00),
        sps.get(3).copied().unwrap_or(0x1F),
        0xFF, // lengthSizeMinusOne = 3
        0xE1, // numOfSPS = 1
    ];
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(0x01); // numOfPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// 判断一个 NALU 是否属于 IDR（类型 5）或 SPS(7)/PPS(8)。
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    if nal.is_empty() {
        0
    } else {
        nal[0] & 0x1F
    }
}

/// 把编码器给的序列头规整成**合法的** AVCDecoderConfigurationRecord。
///
/// Media Foundation 的 `MF_MT_MPEG_SEQUENCE_HEADER` 多半是 Annex-B（起始码分隔的
/// SPS/PPS，甚至不止一个 SPS），直接当 avcC 发出去，服务端会在 `numOfSPS`
/// 字段读到垃圾值并拒收（mediamtx 报 `sps count != 1 is unsupported`，
/// B 站则表现为“显示推流中但直播间无画面”）。
/// 这里统一解析出 SPS/PPS，然后只保留 1 个 SPS + 1 个 PPS 重新拼。
pub fn to_avcc_record(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 8 {
        return None;
    }
    // Annex-B：起始码分隔
    if blob.starts_with(&[0, 0, 0, 1]) || blob.starts_with(&[0, 0, 1]) {
        let nalus = split_nalus(blob);
        let sps = nalus.iter().find(|n| nal_unit_type(n) == 7)?;
        let pps = nalus.iter().find(|n| nal_unit_type(n) == 8)?;
        return Some(build_avcc(sps, pps));
    }
    // 已经是 avcC：解析出第一组 SPS/PPS 后重新规整，避免 numOfSPS>1 或长度字段异常
    if blob[0] == 0x01 {
        let num_sps = (blob[5] & 0x1F) as usize;
        let mut i = 6usize;
        let mut sps: Option<&[u8]> = None;
        for _ in 0..num_sps {
            if i + 2 > blob.len() {
                return None;
            }
            let len = u16::from_be_bytes([blob[i], blob[i + 1]]) as usize;
            i += 2;
            if i + len > blob.len() {
                return None;
            }
            if sps.is_none() {
                sps = Some(&blob[i..i + len]);
            }
            i += len;
        }
        if i >= blob.len() {
            return None;
        }
        let num_pps = blob[i] as usize;
        i += 1;
        let mut pps: Option<&[u8]> = None;
        for _ in 0..num_pps {
            if i + 2 > blob.len() {
                return None;
            }
            let len = u16::from_be_bytes([blob[i], blob[i + 1]]) as usize;
            i += 2;
            if i + len > blob.len() {
                return None;
            }
            if pps.is_none() {
                pps = Some(&blob[i..i + len]);
            }
            i += len;
        }
        return Some(build_avcc(sps?, pps?));
    }
    None
}
