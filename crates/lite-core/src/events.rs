//! 引擎 -> UI 的事件模型（前端通过 Tauri 事件通道订阅）。

use crate::config::CaptureBackendKind;
use crate::stats::StatsSnapshot;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EngineEvent {
    /// 推流已启动
    Started {
        width: u32,
        height: u32,
        fps: u32,
    },
    /// 推流已停止
    Stopped {
        reason: String,
    },
    /// 采集源发生变化（自动降级 / 热切换）
    SourceChanged {
        kind: CaptureBackendKind,
        desc: String,
        reason: String,
    },
    /// 编码器信息
    EncoderInfo {
        name: String,
        hardware: bool,
        codec: String,
    },
    /// 音频状态
    AudioStatus {
        system: bool,
        mic: bool,
        note: String,
    },
    /// 运行统计（每秒一次）
    Stats {
        stats: StatsSnapshot,
    },
    /// 推流连接状态（真实反映服务端是否已接受，而不是“引擎启动了”）
    RtmpStatus {
        /// connecting / publishing / retrying / failed / stopped
        state: String,
        detail: String,
    },
    /// 日志
    Log {
        level: String,
        msg: String,
    },
    /// 错误
    Error {
        msg: String,
    },
}
