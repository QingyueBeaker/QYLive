//! 运行时配置：推流参数、采集参数、音频参数。
//! 所有参数都是纯数据，可序列化，便于前端直接下发。

use serde::{Deserialize, Serialize};

/// 采集后端种类。`Auto` 表示由 CaptureManager 自动降级决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureBackendKind {
    Dxgi,
    Wgc,
    Gdi,
}

impl CaptureBackendKind {
    /// 自动降级顺序：低开销 -> 高开销 -> 万能兜底。
    pub fn degrade_chain() -> [CaptureBackendKind; 3] {
        [
            CaptureBackendKind::Dxgi,
            CaptureBackendKind::Wgc,
            CaptureBackendKind::Gdi,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CaptureBackendKind::Dxgi => "dxgi",
            CaptureBackendKind::Wgc => "wgc",
            CaptureBackendKind::Gdi => "gdi",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CaptureBackendKind::Dxgi => "DXGI 桌面复制",
            CaptureBackendKind::Wgc => "WGC 进程捕获",
            CaptureBackendKind::Gdi => "GDI BitBlt",
        }
    }

    pub fn next(&self) -> Option<CaptureBackendKind> {
        match self {
            CaptureBackendKind::Dxgi => Some(CaptureBackendKind::Wgc),
            CaptureBackendKind::Wgc => Some(CaptureBackendKind::Gdi),
            CaptureBackendKind::Gdi => None,
        }
    }
}

/// 采集源配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// 是否开启自动降级。关闭后严格使用 `locked`。
    pub auto: bool,
    /// 手动锁定的后端（auto=false 或自动链起点）。
    pub locked: Option<CaptureBackendKind>,
    /// 显示器序号（DXGI 使用）。
    pub monitor: usize,
    /// 窗口句柄（WGC / GDI 使用），0 表示主显示器全屏。
    pub hwnd: isize,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            auto: true,
            locked: None,
            monitor: 0,
            hwnd: 0,
        }
    }
}

/// 码率控制模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    /// 固定码率：CBR，带宽平稳，适合平台限码率
    Cbr,
    /// 动态平均码率：ABR / PeakConstrainedVBR，以目标码率为均值、
    /// 画面剧烈变化时临时上冲到峰值，静止时回落，观感比 CBR 好
    Abr,
    /// 纯可变码率：UnconstrainedVBR，质量优先，码率可能大幅超出目标
    Vbr,
}

/// 视频编码参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 关键帧间隔（秒）。GOP = gop_seconds * fps。
    pub gop_seconds: u32,
    pub rate_control: RateControl,
    /// 是否优先尝试 GPU 硬件编码（Media Foundation + D3D11 纹理直通）。
    /// 默认关闭：硬件 MFT 因驱动差异可能不可用，失败会自动回退软件编码。
    #[serde(default)]
    pub prefer_gpu: bool,
    /// 是否优先使用 NVIDIA NVENC 直连编码（nvEncodeAPI）。
    /// 需要 NVIDIA 显卡 + 驱动；不可用时自动回退 Media Foundation。
    #[serde(default)]
    pub prefer_nvenc: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 6000,
            gop_seconds: 2,
            // 默认 ABR：静态画面回落、动态画面适度上冲，观感优于死板的 CBR
            rate_control: RateControl::Abr,
            prefer_gpu: false,
            prefer_nvenc: true,
        }
    }
}

/// 音频参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub enabled: bool,
    /// 系统内录（WASAPI loopback）。
    pub system_enabled: bool,
    pub system_volume: f32,
    /// 麦克风。
    pub mic_enabled: bool,
    pub mic_volume: f32,
    pub bitrate_kbps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            system_enabled: true,
            system_volume: 1.0,
            mic_enabled: false,
            mic_volume: 1.0,
            bitrate_kbps: 128,
        }
    }
}

/// 预览参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewConfig {
    pub enabled: bool,
    /// 预览长边像素上限（低于推流分辨率）。
    pub max_width: u32,
    pub fps: u32,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_width: 640,
            fps: 30,
        }
    }
}

/// 一次开播所需的完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    /// 用户手动粘贴的 RTMP 地址（例如 rtmp://live-push.bilivideo.com/live-bvc/）
    pub rtmp_url: String,
    /// 用户手动粘贴的串流密钥
    pub stream_key: String,
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub preview: PreviewConfig,
    pub source: SourceConfig,
}

impl RunConfig {
    /// 拆分 RTMP URL 为 (host:port, app)。不做任何平台特定处理。
    pub fn split_url(url: &str) -> Result<(String, String), String> {
        let trimmed = url.trim();
        let rest = trimmed
            .strip_prefix("rtmps://")
            .or_else(|| trimmed.strip_prefix("rtmp://"))
            .ok_or_else(|| "RTMP 地址必须以 rtmp:// 或 rtmps:// 开头".to_string())?;
        let (authority, query) = match rest.split_once('?') {
            Some((a, _q)) => (a, ""),
            None => (rest, ""),
        };
        let _ = query;
        let (hostport, app) = match authority.split_once('/') {
            // rtmp://host:1935  —— 合法：app 为空，常见于本地转发服务器（如直播姬的第三方推流入口）
            None => (authority, ""),
            Some((h, a)) => (h, a),
        };
        let hostport = if hostport.contains(':') {
            hostport.to_string()
        } else {
            format!("{hostport}:1935")
        };
        let app = app.trim_end_matches('/').to_string();
        Ok((hostport, app))
    }
}
