//! 帧调度器（核心）：独立于采集与编码的固定帧率时钟。
//!
//! 双分支设计：
//!   主分支（永久启用）：取最新帧 -> NV12 转换 -> H.264 编码 -> FLV -> RTMP
//!   预览分支（动态开关）：复制帧 -> 等比缩小 -> 交付预览窗口；关闭时该分支完全不执行。
//!
//! 采集热切换 / 掉帧期间，调度器仍按帧率产出占位帧，RTMP 时间戳连续，
//! 因此直播间不会因为采集切换而断流。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::capture::VideoFrame;
use crate::convert::{bgra_to_nv12, bgra_scale_to_rgba};
use crate::stats::Stats;

/// 一帧待编码任务。`frame == None` 表示用上一帧/黑帧占位（保证不断流）。
pub struct VideoJob {
    pub frame: Option<Arc<VideoFrame>>,
    pub pts_100ns: i64,
    pub force_key: bool,
}

/// 预览共享状态：Rust 写入，前端轮询读取。
pub struct PreviewState {
    pub enabled: AtomicBool,
    pub seq: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub max_width: AtomicU32,
    pub fps_limit: AtomicU32,
    pub frame: Mutex<Vec<u8>>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            max_width: AtomicU32::new(640),
            fps_limit: AtomicU32::new(30),
            frame: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Serialize)]
pub struct PreviewSnapshot {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
}

impl PreviewState {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if !on {
            // 立刻释放缓冲区，避免关闭后仍占用内存
            if let Ok(mut f) = self.frame.lock() {
                f.clear();
                f.shrink_to_fit();
            }
            self.seq.store(0, Ordering::Relaxed);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 前端轮询：返回 (seq, 宽, 高, RGBA 数据)。
    pub fn take(&self) -> (u64, u32, u32, Vec<u8>) {
        let seq = self.seq.load(Ordering::Acquire);
        let w = self.width.load(Ordering::Relaxed);
        let h = self.height.load(Ordering::Relaxed);
        let data = match self.frame.lock() {
            Ok(f) => f.clone(),
            Err(_) => Vec::new(),
        };
        (seq, w, h, data)
    }
}

pub struct SchedulerHandle {
    pub stop: Arc<AtomicBool>,
    pub join: Option<JoinHandle<()>>,
}

pub struct SchedulerArgs {
    pub fps: u32,
    pub shared: crate::capture::manager::SharedFrame,
    pub job_tx: SyncSender<VideoJob>,
    pub preview: Arc<PreviewState>,
    pub stats: Stats,
    pub stop: Arc<AtomicBool>,
    /// 每 GOP 秒数请求一次关键帧
    pub gop_seconds: u32,
}

pub fn spawn_scheduler(args: SchedulerArgs) -> SchedulerHandle {
    let stop = args.stop.clone();
    let join = std::thread::Builder::new()
        .name("scheduler".into())
        .spawn(move || run(args))
        .expect("创建帧调度线程失败");
    SchedulerHandle {
        stop,
        join: Some(join),
    }
}

fn run(args: SchedulerArgs) {
    let SchedulerArgs {
        fps,
        shared,
        job_tx,
        preview,
        stats,
        stop,
        gop_seconds,
    } = args;

    let fps = fps.max(1);
    let frame_dur = Duration::from_nanos(1_000_000_000u64 / fps as u64);
    let start = Instant::now();
    let mut tick: u64 = 0;
    let gop_ticks = (gop_seconds.max(1) as u64 * fps as u64).max(1);
    let mut last_seq_seen: u64 = u64::MAX;
    let mut preview_counter: u32 = 0;
    let mut preview_buf = Vec::new();
    let mut last_frame: Option<Arc<VideoFrame>> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let target = start + frame_dur.mul_f64(tick as f64);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }

        // 取最新帧（与上一帧同源则视为无新帧，用占位避免时间戳断裂）
        let new_frame = {
            let slot = shared.lock().unwrap();
            match slot.as_ref() {
                Some(f) => {
                    let seq = f.ts_ms;
                    if seq != last_seq_seen {
                        last_seq_seen = seq;
                        Some(f.clone())
                    } else {
                        None
                    }
                }
                None => None,
            }
        };

        let frame_for_job = new_frame.clone().or_else(|| last_frame.clone());
        if let Some(f) = &new_frame {
            last_frame = Some(f.clone());
        }

        // 预览分支：关闭时这里仅一次原子读取，没有任何帧复制与缩放
        if preview.is_enabled() {
            if let Some(f) = frame_for_job.as_ref() {
                let limit = preview.fps_limit.load(Ordering::Relaxed).max(1);
                let stride = (fps / limit).max(1);
                preview_counter = (preview_counter + 1) % stride;
                if preview_counter == 0 {
                    let max_w = preview.max_width.load(Ordering::Relaxed).max(64);
                    let (dw, dh) = if f.width > max_w {
                        let scale = max_w as f32 / f.width as f32;
                        (max_w & !1, ((f.height as f32 * scale) as u32) & !1)
                    } else {
                        ((f.width & !1).max(2), (f.height & !1).max(2))
                    };
                    bgra_scale_to_rgba(&f.data, f.stride, f.width, f.height, dw, dh, &mut preview_buf);
                    if let Ok(mut out) = preview.frame.lock() {
                        *out = preview_buf.clone();
                    }
                    preview.width.store(dw, Ordering::Relaxed);
                    preview.height.store(dh, Ordering::Relaxed);
                    preview.seq.fetch_add(1, Ordering::Release);
                }
            }
        }

        let force_key = tick % gop_ticks == 0;
        let job = VideoJob {
            frame: frame_for_job,
            pts_100ns: (tick as i64 * 10_000_000) / fps as i64,
            force_key,
        };
        // 编码线程落后时丢帧而不是阻塞调度时钟（保证 RTMP 时间戳严格均匀）
        if job_tx.try_send(job).is_err() {
            stats.inc_dropped();
        }

        tick += 1;
    }
}

/// 供编码线程复用：把 BGRA 帧转 NV12（在编码线程做，避免拖慢调度时钟）。
pub fn to_nv12(frame: &VideoFrame, out: &mut Vec<u8>) {
    bgra_to_nv12(&frame.data, frame.stride, frame.width, frame.height, out);
}

/// 生成黑帧占位 NV12。
pub fn placeholder_nv12(width: u32, height: u32, out: &mut Vec<u8>) {
    crate::convert::black_nv12(width, height, out);
}

/// 标记：一次直播结束
pub struct StopToken(pub Arc<AtomicBool>);
