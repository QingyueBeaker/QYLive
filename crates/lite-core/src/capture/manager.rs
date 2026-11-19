//! 采集管理器：负责后端创建、自动降级、直播中热切换。
//!
//! 关键点：管理器始终在**同一个专属线程**中运行（COM / D3D 要求线程固定），
//! 热切换只替换内部后端实例，帧调度器与推流连接完全不受影响 —— 因此不断流。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use super::{create_backend, VideoCapture, VideoFrame};
use crate::config::{CaptureBackendKind, SourceConfig};
use crate::events::EngineEvent;
use crate::stats::Stats;

/// 采集线程与调度器之间共享的“最新一帧”。
pub type SharedFrame = Arc<Mutex<Option<Arc<VideoFrame>>>>;

pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub join: Option<JoinHandle<()>>,
}

/// 启动采集线程。
pub fn spawn_capture(
    cfg: SourceConfig,
    fps: u32,
    shared: SharedFrame,
    stats: Stats,
    events: UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> CaptureHandle {
    let thread_stop = stop.clone();
    let join = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run(cfg, fps, shared, stats, events, thread_stop))
        .expect("创建采集线程失败");
    CaptureHandle {
        stop,
        join: Some(join),
    }
}

fn emit(events: &UnboundedSender<EngineEvent>, ev: EngineEvent) {
    let _ = events.send(ev);
}

fn run(
    cfg: SourceConfig,
    fps: u32,
    shared: SharedFrame,
    stats: Stats,
    events: UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }

    // 起始后端 + 降级链
    let start_kind = if cfg.auto {
        cfg.locked.unwrap_or(CaptureBackendKind::Dxgi)
    } else {
        cfg.locked.unwrap_or(CaptureBackendKind::Dxgi)
    };
    let mut chain: Vec<CaptureBackendKind> = Vec::new();
    let mut cur = Some(start_kind);
    while let Some(k) = cur {
        chain.push(k);
        cur = if cfg.auto { k.next() } else { None };
    }

    let mut idx = 0usize;
    let mut backend: Option<Box<dyn VideoCapture>> = None;

    // 逐级尝试启动，直到有一层可用
    while idx < chain.len() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let kind = chain[idx];
        let mut b = create_backend(kind, cfg.monitor, cfg.hwnd);
        match b.start().and_then(|_| wait_first_frame(&mut *b, 2000)) {
            Ok(()) => {
                emit(
                    &events,
                    EngineEvent::SourceChanged {
                        kind,
                        desc: b.describe(),
                        reason: if idx == 0 {
                            "初始采集".into()
                        } else {
                            format!("自动降级（第 {idx} 级）")
                        },
                    },
                );
                backend = Some(b);
                break;
            }
            Err(e) => {
                log::warn!("采集后端 {} 启动失败：{e}", kind.as_str());
                stats.add_capture_failure();
                let _ = b.stop();
                idx += 1;
            }
        }
    }

    let mut backend = match backend {
        Some(b) => b,
        None => {
            emit(
                &events,
                EngineEvent::Error {
                    msg: "所有采集后端均不可用（DXGI / WGC / GDI 全部失败）".into(),
                },
            );
            return;
        }
    };

    let frame_budget = Duration::from_millis((1000 / fps.max(1)) as u64);
    let mut black_since: Option<Instant> = None;
    let mut fps_window_start = Instant::now();
    let mut fps_window_frames = 0u32;

    while !stop.load(Ordering::Relaxed) {
        let timeout = frame_budget.as_millis().max(4) as u32;
        match backend.get_frame(timeout) {
            Ok(Some(frame)) => {
                let is_black = frame.is_black();
                if is_black {
                    let since = *black_since.get_or_insert_with(Instant::now);
                    // 连续 1.5 秒黑屏空帧：判定当前采集路径失效，热切换下一级
                    if since.elapsed() > Duration::from_millis(1500) && cfg.auto {
                        black_since = None;
                        match degrade(&mut backend, &chain, &mut idx, &cfg, &events, &stats) {
                            Some(b) => {
                                backend = b;
                                continue;
                            }
                            None => {
                                // 已无更优后端，保持现状继续复用帧
                            }
                        }
                    }
                } else {
                    black_since = None;
                }

                {
                    let mut slot = shared.lock().unwrap();
                    *slot = Some(Arc::new(frame));
                }
                fps_window_frames += 1;
                if fps_window_start.elapsed() >= Duration::from_secs(1) {
                    stats.set_capture_fps(fps_window_frames as f64 / fps_window_start.elapsed().as_secs_f64());
                    fps_window_start = Instant::now();
                    fps_window_frames = 0;
                }
            }
            Ok(None) => {
                // 超时无新帧：静置
            }
            Err(e) => {
                log::warn!("采集失败：{e}");
                stats.add_capture_failure();
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = backend.stop();
                match degrade(&mut backend, &chain, &mut idx, &cfg, &events, &stats) {
                    Some(b) => backend = b,
                    None => {
                        // 已到最后一层：重启当前后端
                        std::thread::sleep(Duration::from_millis(500));
                        if backend.start().is_ok() {
                            continue;
                        }
                        emit(
                            &events,
                            EngineEvent::Error {
                                msg: "采集已彻底失效，直播已中断".into(),
                            },
                        );
                        break;
                    }
                }
                continue;
            }
        }

        // 采集状态只在变化时上报，避免每 500ms 刷一条事件干扰 UI 与日志
    }

    let _ = backend.stop();
    unsafe {
        windows::Win32::System::Com::CoUninitialize();
    }
}

/// 等待首个有效帧，判定启动是否成功（2 秒无帧 -> 失败）。
fn wait_first_frame(backend: &mut dyn VideoCapture, budget_ms: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    while Instant::now() < deadline {
        if !backend.is_alive() {
            return Err("后端已失活".into());
        }
        match backend.get_frame(120) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => continue,
            Err(e) => return Err(e),
        }
    }
    Err("2 秒内没有有效帧".into())
}

/// 热切换：销毁当前后端，按降级链创建下一个可用后端。
fn degrade(
    backend: &mut Box<dyn VideoCapture>,
    chain: &[CaptureBackendKind],
    idx: &mut usize,
    cfg: &SourceConfig,
    events: &UnboundedSender<EngineEvent>,
    stats: &Stats,
) -> Option<Box<dyn VideoCapture>> {
    let _ = backend.stop();
    if !cfg.auto {
        // 手动锁定模式：只重启同一后端
        let kind = chain.get(*idx).copied().unwrap_or(CaptureBackendKind::Gdi);
        let mut b = create_backend(kind, cfg.monitor, cfg.hwnd);
        if b.start().is_ok() {
            return Some(b);
        }
        return None;
    }

    let mut next = *idx + 1;
    while next < chain.len() {
        let kind = chain[next];
        let mut b = create_backend(kind, cfg.monitor, cfg.hwnd);
        match b.start() {
            Ok(()) => {
                *idx = next;
                stats.inc_switch();
                emit(
                    events,
                    EngineEvent::SourceChanged {
                        kind,
                        desc: b.describe(),
                        reason: "热切换（推流未中断）".into(),
                    },
                );
                log::info!("采集热切换到 {}", kind.as_str());
                return Some(b);
            }
            Err(e) => {
                log::warn!("热切换 {} 失败：{e}", kind.as_str());
                let _ = b.stop();
                next += 1;
            }
        }
    }
    None
}
