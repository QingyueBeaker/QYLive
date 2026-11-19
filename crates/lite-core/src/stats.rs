//! 推流运行统计，供 UI 实时刷新。全部为原子计数，无锁读取。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct StatsInner {
    pub video_frames: AtomicU64,
    pub video_dropped: AtomicU64,
    pub audio_frames: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_read: AtomicU64,
    pub capture_failures: AtomicU64,
    pub switch_count: AtomicU64,
    pub reconnect_count: AtomicU64,
    pub encoder_fps_x10: AtomicU64,
    pub capture_fps_x10: AtomicU64,
    pub ack_bytes: AtomicU64,
    pub running: AtomicBool,
}

#[derive(Clone, Default)]
pub struct Stats {
    inner: Arc<StatsInner>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StatsSnapshot {
    pub video_frames: u64,
    pub video_dropped: u64,
    pub audio_frames: u64,
    pub bytes_sent: u64,
    pub capture_failures: u64,
    pub switch_count: u64,
    pub reconnect_count: u64,
    pub encoder_fps: f64,
    pub capture_fps: f64,
    pub kbps: f64,
    pub running: bool,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_video(&self) {
        self.inner.video_frames.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_dropped(&self) {
        self.inner.video_dropped.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_audio(&self) {
        self.inner.audio_frames.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_bytes_sent(&self, n: u64) {
        self.inner.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_bytes_read(&self, n: u64) {
        self.inner.bytes_read.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_capture_failure(&self) {
        self.inner.capture_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_switch(&self) {
        self.inner.switch_count.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_reconnect(&self) {
        self.inner.reconnect_count.fetch_add(1, Ordering::Relaxed);
    }
    pub fn set_encoder_fps(&self, fps: f64) {
        self.inner
            .encoder_fps_x10
            .store((fps * 10.0) as u64, Ordering::Relaxed);
    }
    pub fn set_capture_fps(&self, fps: f64) {
        self.inner
            .capture_fps_x10
            .store((fps * 10.0) as u64, Ordering::Relaxed);
    }
    pub fn set_running(&self, running: bool) {
        self.inner.running.store(running, Ordering::Relaxed);
    }
    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Relaxed)
    }

    /// 取走已发送字节数（用于计算瞬时码率，写线程每秒调用一次）。
    pub fn take_bytes_since_last(&self) -> u64 {
        self.inner
            .bytes_sent
            .swap(self.inner.ack_bytes.load(Ordering::Relaxed), Ordering::Relaxed)
    }

    pub fn snapshot(&self, kbps: f64) -> StatsSnapshot {
        let inner = &self.inner;
        StatsSnapshot {
            video_frames: inner.video_frames.load(Ordering::Relaxed),
            video_dropped: inner.video_dropped.load(Ordering::Relaxed),
            audio_frames: inner.audio_frames.load(Ordering::Relaxed),
            bytes_sent: inner.bytes_sent.load(Ordering::Relaxed),
            capture_failures: inner.capture_failures.load(Ordering::Relaxed),
            switch_count: inner.switch_count.load(Ordering::Relaxed),
            reconnect_count: inner.reconnect_count.load(Ordering::Relaxed),
            encoder_fps: inner.encoder_fps_x10.load(Ordering::Relaxed) as f64 / 10.0,
            capture_fps: inner.capture_fps_x10.load(Ordering::Relaxed) as f64 / 10.0,
            kbps,
            running: inner.running.load(Ordering::Relaxed),
        }
    }
}
