//! 真实推流联调（只从环境变量读取地址与密钥，不写盘、不内置任何平台信息）。
//!
//!   $env:LITE_URL="rtmp://..."; $env:LITE_KEY="...";
//!   cargo run --release -p lite-core --example live_test
//!
//! 运行 10 秒后自动停播，用于复现 / 验证真实服务端链路。

use std::sync::Arc;
use std::time::{Duration, Instant};

use lite_core::config::{AudioConfig, PreviewConfig, RateControl, RunConfig, SourceConfig, VideoConfig};
use lite_core::session;

/// 只发布、不发媒体帧的空转探针：能判明是“会话保活问题”还是“媒体帧不合法”。
fn idle_probe(url: &str, key: &str) {
    use lite_core::stats::Stats;
    use lite_core::stream::flv;
    use lite_core::stream::rtmp::RtmpConnection;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio 运行时");
    rt.block_on(async move {
        let (host, app) = RunConfig::split_url(url).expect("推流地址解析失败");
        let stats = Stats::new();
        let mut conn = match RtmpConnection::connect(&host, &app, key, &stats).await {
            Ok(c) => c,
            Err(e) => {
                println!("[idle] 连接/发布失败: {e}");
                return;
            }
        };
        println!("[idle] 已 publish，空转 10 秒（不发任何媒体帧）…");
        tokio::time::sleep(Duration::from_secs(10)).await;
        let tag = flv::metadata(1280, 720, 30, 3000, 128);
        match conn.write_tag(&tag, 0).await {
            Ok(_) => println!("[idle] 空转后写入成功 → 会话保活正常，问题在媒体帧"),
            Err(e) => println!("[idle] 空转后写入失败 → 服务端在 publish 后主动断开: {e}"),
        }
    });
}

fn main() {
    // 极简 stderr logger（引擎内部用 log::warn!/info!）
    struct Simple;
    impl log::Log for Simple {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            eprintln!("[{}] {} {}", r.level(), r.target(), r.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: Simple = Simple;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);

    // 让 panic 打出完整信息（release 下 profile 是 panic=abort，但仍会打印消息）
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {info}");
        eprintln!("[PANIC] backtrace:\n{}", std::backtrace::Backtrace::force_capture());
    }));

    let url = std::env::var("LITE_URL").expect("缺少 LITE_URL");
    let key = std::env::var("LITE_KEY").expect("缺少 LITE_KEY");

    // LITE_IDLE=1：只建立连接并 publish，不发送任何媒体帧，用于判断“发布后能否保活”
    if std::env::var("LITE_IDLE").ok().as_deref() == Some("1") {
        idle_probe(&url, &key);
        return;
    }

    let audio_on = std::env::var("LITE_AUDIO").map(|v| v != "0").unwrap_or(true);
    let cfg = RunConfig {
        rtmp_url: url,
        stream_key: key,
        video: VideoConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 3000,
            gop_seconds: 2,
            rate_control: RateControl::Cbr,
        },
        audio: AudioConfig {
            enabled: audio_on,
            system_enabled: audio_on,
            mic_enabled: false,
            ..AudioConfig::default()
        },
        preview: PreviewConfig::default(),
        source: SourceConfig::default(),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let preview = Arc::new(lite_core::PreviewState::default());

    let mut sess = rt
        .block_on(async { session::start(cfg, tx, preview) })
        .expect("启动失败");
    let stats = sess.stats.clone();
    println!("[live] 已连接，开始推流 10 秒");

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        while let Ok(ev) = rx.try_recv() {
            println!("[live] 事件：{ev:?}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    sess.stop();
    let snap = stats.snapshot(0.0);
    println!(
        "[live] 结束：视频 {} 帧 / 丢 {}，音频 {} 帧，发送 {} KB，重连 {} 次",
        snap.video_frames,
        snap.video_dropped,
        snap.audio_frames,
        snap.bytes_sent / 1024,
        snap.reconnect_count
    );
}
