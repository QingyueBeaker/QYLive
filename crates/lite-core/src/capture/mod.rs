//! 视频采集层：统一 Trait 抽象 + 多后端实现 + 自动降级管理。
//!
//! 所有后端都是同步阻塞式（D3D/GDI 与 COM 都要求调用线程固定），
//! 因此必须在同一个专属线程里创建、使用与销毁；跨线程只通过通道传递帧数据。

pub mod dxgi;
pub mod gdi;
pub mod manager;
pub mod wgc;

use crate::config::CaptureBackendKind;

/// 一帧 BGRA 画面。
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// BGRA8888，长度为 stride * height
    pub data: Vec<u8>,
    /// 采集侧单调时间戳（毫秒）
    pub ts_ms: u64,
}

impl VideoFrame {
    pub fn is_black(&self) -> bool {
        // 采样判断：用于识别“空帧/黑屏”，避免为每帧全量扫描像素。
        let mut min = 255u8;
        let mut max = 0u8;
        let step = (self.data.len() / 4096).max(4);
        let mut i = 0;
        while i + 3 < self.data.len() {
            let v = self.data[i];
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
            i += step;
        }
        max.saturating_sub(min) < 6
    }
}

/// 统一采集后端接口。
pub trait VideoCapture: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<(), String>;
    /// 阻塞式取帧，超时返回 None。
    fn get_frame(&mut self, timeout_ms: u32) -> Result<Option<VideoFrame>, String>;
    fn is_alive(&self) -> bool;
    fn kind(&self) -> CaptureBackendKind;
    fn describe(&self) -> String;
    /// 该后端失败后是否支持热切换到下一个
    fn can_degrade(&self) -> bool {
        self.kind().next().is_some()
    }
}

/// 依据配置创建指定后端实例（不做 start）。
pub fn create_backend(
    kind: CaptureBackendKind,
    monitor: usize,
    hwnd: isize,
) -> Box<dyn VideoCapture> {
    match kind {
        CaptureBackendKind::Dxgi => Box::new(dxgi::DxgiCapture::new(monitor)),
        CaptureBackendKind::Wgc => Box::new(wgc::WgcCapture::new(hwnd, monitor)),
        CaptureBackendKind::Gdi => Box::new(gdi::GdiCapture::new(hwnd, monitor)),
    }
}
