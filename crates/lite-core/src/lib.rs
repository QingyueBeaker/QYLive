//! Lite RTMP Streamer 核心引擎。
//!
//! 模块划分（全部通过 tokio mpsc / std mpsc 通信，模块之间零直接调用）：
//!   capture  —— 三层采集后端 + 自动降级 + 热切换
//!   audio    —— WASAPI 系统内录/麦克风 + 混音
//!   encode   —— Media Foundation H.264 / AAC（硬件优先）
//!   scheduler—— 固定帧率时钟 + 预览分支动态开关
//!   stream   —— 自研 RTMP 客户端 + FLV 封装
//!   session  —— 上述模块的装配与生命周期

pub mod audio;
pub mod capture;
pub mod config;
pub mod convert;
pub mod encode;
pub mod events;
pub mod scheduler;
pub mod session;
pub mod stats;
pub mod stream;

pub use config::{AudioConfig, CaptureBackendKind, PreviewConfig, RateControl, RunConfig, SourceConfig, VideoConfig};
pub use events::EngineEvent;
pub use scheduler::{PreviewSnapshot, PreviewState};
pub use session::{start, StreamSession};
pub use stats::{Stats, StatsSnapshot};

/// 单调递增毫秒时间戳（相对进程启动），用于采集帧序号。
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}
