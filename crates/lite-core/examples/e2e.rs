//! 端到端联调：本机起一个最小 RTMP 握手接收器，跑完整直播流水线。
//!
//!   cargo run -p lite-core --example e2e
//!
//! 验证：采集 -> 帧调度 -> H.264 -> FLV -> RTMP 写；音频 -> AAC -> FLV -> RTMP 写。
//! 只连本机 127.0.0.1，不涉及任何外部服务。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lite_core::config::{
    AudioConfig, CaptureBackendKind, PreviewConfig, RateControl, RunConfig, SourceConfig, VideoConfig,
};
use lite_core::session;
use lite_core::stats::Stats;

fn main() {
    let port = 11935u16;
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("端口被占用");
    let received = Arc::new(AtomicU64::new(0));
    let rec = received.clone();

    // 极简 RTMP 服务端：只做握手 + 收字节，不解析语义
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
            let mut c0c1 = vec![0u8; 1537];
            if sock.read_exact(&mut c0c1).is_err() {
                return;
            }
            // S0 + S1 + S2
            let mut s = Vec::with_capacity(1 + 1536 * 2);
            s.push(3u8);
            s.extend_from_slice(&c0c1[1..]); // S1 直接用客户端 C1 的内容填充
            s.extend_from_slice(&c0c1[1..]); // S2 回显
            if sock.write_all(&s).is_err() {
                return;
            }
            let mut buf = vec![0u8; 64 * 1024];
            let mut n_total = 0u64;
            loop {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        n_total += n as u64;
                        rec.store(n_total, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // 读超时后退避，等待更多数据
                    }
                }
            }
        }
    });

    let cfg = RunConfig {
        rtmp_url: format!("rtmp://127.0.0.1:{port}/live"),
        stream_key: "e2e-test-key".to_string(),
        video: VideoConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 3000,
            gop_seconds: 2,
            rate_control: RateControl::Cbr,
            prefer_gpu: false,
            prefer_nvenc: std::env::var("LITE_NVENC").map(|v| v != "0").unwrap_or(true),
        },
        audio: AudioConfig {
            enabled: true,
            system_enabled: true,
            system_volume: 1.0,
            mic_enabled: false,
            mic_volume: 1.0,
            bitrate_kbps: 128,
        },
        preview: PreviewConfig::default(),
        source: SourceConfig {
            auto: true,
            locked: Some(CaptureBackendKind::Dxgi),
            monitor: 0,
            hwnd: 0,
        },
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建 tokio runtime 失败");

    let stats = Stats::new();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let preview = Arc::new(lite_core::PreviewState::default());

    let started = Instant::now();
    let mut sess = rt
        .block_on(async { session::start(cfg, events_tx, preview) })
        .expect("启动直播失败");
    let s = sess.stats.clone();
    let _ = &stats;
    println!("[E2E] 直播已启动");

    // 运行 8 秒，期间打印事件
    let run_until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < run_until {
        if let Ok(ev) = events_rx.try_recv() {
            println!("[E2E] 事件：{ev:?}");
        }
        std::thread::sleep(Duration::from_millis(500));
        let snap = s.snapshot(0.0);
        println!(
            "[E2E] {:.1}s 视频帧={} 丢帧={} 音频帧={} 已发={}KB 服务端收到={}KB",
            started.elapsed().as_secs_f64(),
            snap.video_frames,
            snap.video_dropped,
            snap.audio_frames,
            snap.bytes_sent / 1024,
            received.load(Ordering::Relaxed) / 1024
        );
    }

    sess.stop();
    std::thread::sleep(Duration::from_millis(600));
    let snap = s.snapshot(0.0);
    println!(
        "[E2E] 结束：视频 {} 帧、丢帧 {}、音频 {} 帧、发送 {} KB、服务端收到 {} KB",
        snap.video_frames,
        snap.video_dropped,
        snap.audio_frames,
        snap.bytes_sent / 1024,
        received.load(Ordering::Relaxed) / 1024
    );
    println!("[E2E] 判定：{}", if received.load(Ordering::Relaxed) > 10_000 { "通过（有数据落对端）" } else { "失败（对端未收到数据）" });
}
