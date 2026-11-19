//! 直播会话：把采集、音频、调度、编码、推流串成一条可启停的流水线。
//!
//! 线程模型：
//!   capture 线程（COM/D3D 固定线程） -> 共享最新帧
//!   scheduler 线程（固定帧率时钟）   -> VideoJob 队列 -> encoder 线程
//!   audio 线程（WASAPI）-> Mixer -> AAC 线程
//!   encoder / AAC 线程 -> FLV tag 通道 -> tokio RTMP 写线程
//!
//! 只有 RTMP 写线程是 async 任务，其余都是裸线程，避免 tokio 调度抖动影响帧率。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::audio::mixer::{samples_to_ms, Mixer, AAC_FRAME_SAMPLES, TARGET_RATE};
use crate::audio::{spawn_audio, AudioVolumes};
use crate::capture::manager::{spawn_capture, SharedFrame};
use crate::capture::VideoFrame;
use crate::config::{AudioConfig, RunConfig, VideoConfig};
use crate::encode::aac::MfAacEncoder;
use crate::encode::nvenc::VideoEncoder;
use crate::events::EngineEvent;
use crate::scheduler::{spawn_scheduler, PreviewState, SchedulerArgs, VideoJob};
use crate::stats::Stats;
use crate::stream::flv::{self, FlvTag};
use crate::stream::rtmp::publish_loop;

pub struct StreamSession {
    pub stop: Arc<AtomicBool>,
    pub stats: Stats,
    pub preview: Arc<PreviewState>,
    pub volumes: AudioVolumes,
    threads: Vec<JoinHandle<()>>,
    rtmp_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    finished: bool,
}

impl StreamSession {
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

/// 启动一次直播。返回的 Session 需由调用方持有，调用 `stop()` 干净释放。
pub fn start(
    cfg: RunConfig,
    events: UnboundedSender<EngineEvent>,
    preview: Arc<PreviewState>,
) -> Result<StreamSession, String> {
    let (host_port, app) = RunConfig::split_url(&cfg.rtmp_url)?;
    let key = cfg.stream_key.trim().to_string();
    if key.is_empty() {
        return Err("串流密钥不能为空".into());
    }

    let video = normalize_video(&cfg.video)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Stats::new();
    stats.set_running(true);

    let shared: SharedFrame = Arc::new(Mutex::new(None));
    let mixer = Arc::new(Mutex::new(Mixer::new()));
    let volumes = AudioVolumes::new(&cfg.audio);

    let (tag_tx, tag_rx) = unbounded_channel::<FlvTag>();
    let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<VideoJob>(4);

    let mut threads: Vec<JoinHandle<()>> = Vec::new();

    // 采集
    let cap = spawn_capture(
        cfg.source.clone(),
        video.fps,
        shared.clone(),
        stats.clone(),
        events.clone(),
        stop.clone(),
    );
    threads.push(cap.join.unwrap());

    // 音频采集
    let audio = spawn_audio(
        cfg.audio.clone(),
        mixer.clone(),
        volumes.clone(),
        stats.clone(),
        events.clone(),
        stop.clone(),
    );
    if let Some(j) = audio.join {
        threads.push(j);
    }

    // 视频编码
    {
        let video = video.clone();
        let tag_tx = tag_tx.clone();
        let events = events.clone();
        let stats = stats.clone();
        let stop = stop.clone();
        let j = std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || encoder_thread(video, job_rx, tag_tx, events, stats, stop))
            .map_err(|e| format!("创建编码线程失败: {e}"))?;
        threads.push(j);
    }

    // 音频编码
    if cfg.audio.enabled {
        let tag_tx = tag_tx.clone();
        let events = events.clone();
        let stats = stats.clone();
        let stop = stop.clone();
        let audio_cfg = cfg.audio.clone();
        let mixer = mixer.clone();
        let j = std::thread::Builder::new()
            .name("aac".into())
            .spawn(move || audio_thread(audio_cfg, mixer, tag_tx, events, stats, stop))
            .map_err(|e| format!("创建音频编码线程失败: {e}"))?;
        threads.push(j);
    }

    // 帧调度
    let sch = spawn_scheduler(SchedulerArgs {
        fps: video.fps,
        shared,
        job_tx,
        preview: preview.clone(),
        stats: stats.clone(),
        stop: stop.clone(),
        gop_seconds: video.gop_seconds,
    });
    threads.push(sch.join.unwrap());

    log::info!(
        "直播已启动 {}x{}@{} -> {}/{}",
        video.width,
        video.height,
        video.fps,
        host_port,
        app
    );

    // 推流
    let rtmp_task = tokio::spawn(publish_loop(
        host_port,
        app,
        key,
        tag_rx,
        stats.clone(),
        events.clone(),
    ));

    // 统计上报
    {
        let stats = stats.clone();
        let events = events.clone();
        let stop = stop.clone();
        let j = std::thread::Builder::new()
            .name("stats".into())
            .spawn(move || stats_thread(stats, events, stop))
            .map_err(|e| format!("创建统计线程失败: {e}"))?;
        threads.push(j);
    }

    let _ = events.send(EngineEvent::Started {
        width: video.width,
        height: video.height,
        fps: video.fps,
    });

    let _ = cfg.preview.enabled;

    Ok(StreamSession {
        stop,
        stats,
        preview,
        volumes,
        threads,
        rtmp_task: Some(rtmp_task),
        finished: false,
    })
}

impl StreamSession {
    /// 干净停播：停采集/编码/调度 -> 关闭 tag 通道 -> 等待线程与 RTMP 任务退出。
    pub fn stop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.stop.store(true, Ordering::Relaxed);
        self.stats.set_running(false);
        self.preview.set_enabled(false);

        // 采集/音频线程自己会检查 stop 标志退出；这里逐个 join
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
        // 所有 tag 发送端已随线程析构，RTMP 任务会在通道关闭后自然结束
        if let Some(task) = self.rtmp_task.take() {
            task.abort();
        }
    }

    pub fn join(mut self) {
        self.stop();
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn normalize_video(v: &VideoConfig) -> Result<VideoConfig, String> {
    let mut out = v.clone();
    if out.width < 32 || out.height < 32 {
        return Err("分辨率过低".into());
    }
    // H.264 4:2:0 要求偶数宽高
    out.width &= !1;
    out.height &= !1;
    if out.fps == 0 || out.fps > 240 {
        return Err("帧率超出范围（1-240）".into());
    }
    if out.bitrate_kbps < 100 {
        return Err("码率过低（建议 1000 以上）".into());
    }
    if out.gop_seconds == 0 {
        out.gop_seconds = 2;
    }
    Ok(out)
}

fn encoder_thread(
    video: VideoConfig,
    job_rx: std::sync::mpsc::Receiver<VideoJob>,
    tag_tx: UnboundedSender<FlvTag>,
    events: UnboundedSender<EngineEvent>,
    stats: Stats,
    stop: Arc<AtomicBool>,
) {
    unsafe {
        let _ =
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }

    let mut enc = match VideoEncoder::new(&video) {
        Ok(e) => e,
        Err(err) => {
            let _ = events.send(EngineEvent::Error {
                msg: format!("H.264 编码器初始化失败：{err}"),
            });
            stop.store(true, Ordering::Relaxed);
            return;
        }
    };

    let _ = events.send(EngineEvent::EncoderInfo {
        name: enc.name(),
        hardware: enc.is_hardware(),
        codec: "H.264".into(),
    });

    // 元数据 + 序列头
    let _ = tag_tx.send(flv::metadata(
        video.width,
        video.height,
        video.fps,
        video.bitrate_kbps,
        128,
    ));
    let mut sent_sequence_header = false;
    if let Some(ex) = enc.extradata() {
        let _ = tag_tx.send(flv::video_sequence_header(&ex));
        sent_sequence_header = true;
    }

    let mut nv12: Vec<u8> = Vec::new();
    let mut last_nv12: Vec<u8> = Vec::new();
    let mut scratch_bgra: Vec<u8> = Vec::new();
    let mut fps_window = Instant::now();
    let mut fps_frames = 0u32;
    // NVENC 走 GPU 色彩转换：直接吃 BGRA，省掉每帧 3.7ms 的 CPU 逐像素转换
    let bgra_mode = enc.prefers_bgra();
    let mut last_frame: Option<Arc<VideoFrame>> = None;

    while !stop.load(Ordering::Relaxed) {
        let job = match job_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(j) => j,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };

        let encoded = if bgra_mode {
            if job.frame.is_some() {
                last_frame = job.frame;
            }
            let Some(frame) = last_frame.as_ref() else {
                continue;
            };
            if job.force_key {
                enc.request_keyframe();
            }
            enc.encode_bgra(&frame.data, frame.stride, frame.width, frame.height)
        } else {

        match job.frame {
            Some(frame) => {
                if frame.width != video.width || frame.height != video.height {
                    crate::convert::bgra_scale_to_bgra(
                        &frame.data,
                        frame.stride,
                        frame.width,
                        frame.height,
                        video.width,
                        video.height,
                        &mut scratch_bgra,
                    );
                    nv12.resize((video.width as usize) * (video.height as usize) * 3 / 2, 0);
                    crate::convert::bgra_to_nv12(
                        &scratch_bgra,
                        (video.width * 4) as usize,
                        video.width,
                        video.height,
                        &mut nv12,
                    );
                } else {
                    nv12.resize((video.width as usize) * (video.height as usize) * 3 / 2, 0);
                    crate::convert::bgra_to_nv12(
                        &frame.data,
                        frame.stride,
                        video.width,
                        video.height,
                        &mut nv12,
                    );
                }
                last_nv12.clear();
                last_nv12.extend_from_slice(&nv12);
            }
            None => {
                // 无新帧：复用上一帧（占位），时间戳照常推进
                if last_nv12.is_empty() {
                    crate::scheduler::placeholder_nv12(video.width, video.height, &mut last_nv12);
                }
                nv12.clear();
                nv12.extend_from_slice(&last_nv12);
            }
        }

        if job.force_key {
            enc.request_keyframe();
        }

        enc.encode(&nv12, job.pts_100ns)
        };

        match encoded {
            Ok(frames) => {
                if !sent_sequence_header {
                    if let Some(ex) = enc.extradata() {
                        let _ = tag_tx.send(flv::video_sequence_header(&ex));
                        sent_sequence_header = true;
                    }
                }
                for f in frames {
                    let ts_ms = (job.pts_100ns / 10_000) as u32;
                    stats.inc_video();
                    let _ = tag_tx.send(flv::video_frame(f.data, ts_ms, f.keyframe));
                }
                fps_frames += 1;
                if fps_window.elapsed() >= Duration::from_secs(1) {
                    stats.set_encoder_fps(fps_frames as f64 / fps_window.elapsed().as_secs_f64());
                    fps_window = Instant::now();
                    fps_frames = 0;
                }
            }
            Err(e) => {
                log::warn!("编码失败：{e}");
                let _ = events.send(EngineEvent::Log {
                    level: "warn".into(),
                    msg: format!("编码失败：{e}"),
                });
            }
        }
    }

    let _ = enc.finish();
    unsafe {
        windows::Win32::System::Com::CoUninitialize();
    }
}

fn audio_thread(
    cfg: AudioConfig,
    mixer: Arc<Mutex<Mixer>>,
    tag_tx: UnboundedSender<FlvTag>,
    events: UnboundedSender<EngineEvent>,
    stats: Stats,
    stop: Arc<AtomicBool>,
) {
    unsafe {
        let _ =
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut enc = match MfAacEncoder::new(cfg.bitrate_kbps) {
        Ok(e) => e,
        Err(e) => {
            let _ = events.send(EngineEvent::Log {
                level: "warn".into(),
                msg: format!("AAC 编码器不可用，本次将只推视频：{e}"),
            });
            return;
        }
    };

    let _ = tag_tx.send(flv::audio_sequence_header(&enc.asc()));

    let mut block: Vec<i16> = Vec::with_capacity(AAC_FRAME_SAMPLES * 2);
    let mut starve_ticks = 0u32;
    let mut starve_anchor: Option<Instant> = None;
    let mut silence_pushed = 0u32;

    while !stop.load(Ordering::Relaxed) {
        let pulled = match mixer.lock() {
            Ok(mut m) => {
                if let Some(ts_samples) = m.take_aac_frame(&mut block) {
                    Some((ts_samples, block.clone()))
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        match pulled {
            Some((ts_samples, samples)) => {
                starve_ticks = 0;
                starve_anchor = None;
                silence_pushed = 0;
                let ts_ms = samples_to_ms(ts_samples, TARGET_RATE);
                match enc.encode(&samples) {
                    Ok(packets) => {
                        for p in packets {
                            let _ = tag_tx.send(flv::audio_frame(p, ts_ms));
                            stats.inc_audio();
                        }
                    }
                    Err(e) => log::warn!("AAC 编码失败：{e}"),
                }
            }
            None => {
                // 静默补帧：按真实墙上时间补，保证音频时间轴与视频同步，
                // 不会因为“系统当前没有声音”而让音频时间戳落后于视频。
                if starve_ticks < 3 {
                    starve_ticks += 1;
                } else {
                    let anchor = *starve_anchor.get_or_insert_with(Instant::now);
                    let expected = (anchor.elapsed().as_secs_f64()
                        * TARGET_RATE as f64
                        / AAC_FRAME_SAMPLES as f64) as u32;
                    while silence_pushed < expected {
                        let silence = vec![0.0f32; AAC_FRAME_SAMPLES * 2];
                        if let Ok(mut m) = mixer.lock() {
                            // 补静音只补在系统轨：它代表“总时间轴”，麦克风轨由采集线程自己产数据
                            m.push_stereo(crate::audio::mixer::Track::System, &silence);
                        }
                        silence_pushed += 1;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn stats_thread(
    stats: Stats,
    events: UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut last_bytes = 0u64;
    let mut last_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(1000));
        let snap_bytes = stats.snapshot(0.0).bytes_sent;
        let dt = last_at.elapsed().as_secs_f64().max(0.001);
        let kbps = ((snap_bytes.saturating_sub(last_bytes)) as f64 * 8.0) / 1000.0 / dt;
        last_bytes = snap_bytes;
        last_at = Instant::now();
        let _ = events.send(EngineEvent::Stats {
            stats: stats.snapshot(kbps),
        });
    }
}
