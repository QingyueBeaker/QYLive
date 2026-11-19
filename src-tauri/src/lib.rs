//! Tauri 着色层：只做窗口、命令与事件转发，业务全部在 lite-core 引擎中。

use std::sync::{Arc, Mutex};

use lite_core::PreviewState;
use lite_core::StreamSession;
use tauri::{Emitter, Manager};

mod commands;

pub struct AppState {
    pub session: Mutex<Option<StreamSession>>,
    pub preview: Arc<PreviewState>,
}

pub fn run() {
    let _ = env_logger_init();

    tauri::Builder::default()
        .setup(|app| {
            let preview = Arc::new(PreviewState::default());
            app.manage(AppState {
                session: Mutex::new(None),
                preview,
            });
            // 诊断开关：设置 LITE_PANEL_TEST=1 时启动即打开主播后台（系统浏览器），便于排查
            if std::env::var("LITE_PANEL_TEST").ok().as_deref() == Some("1") {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    match commands::open_live_panel() {
                        Ok(()) => eprintln!("[panel-test] 已请求打开主播后台"),
                        Err(e) => eprintln!("[panel-test] 打开失败：{e}"),
                    }
                });
            }
            // 诊断开关：设置 LITE_STREAM_TEST=1 时启动即自动调用 start_stream，
            // 用于验证“点开始推流是否还会闪退”（只在本地排查时使用）
            if std::env::var("LITE_STREAM_TEST").ok().as_deref() == Some("1") {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let url = std::env::var("LITE_URL").unwrap_or_else(|_| "rtmp://127.0.0.1:1/probe".into());
                    let key = std::env::var("LITE_KEY").unwrap_or_else(|_| "probe".into());
                    let cfg: lite_core::RunConfig = lite_core::RunConfig {
                        rtmp_url: url,
                        stream_key: key,
                        video: lite_core::VideoConfig::default(),
                        audio: lite_core::AudioConfig::default(),
                        preview: lite_core::PreviewConfig::default(),
                        source: lite_core::SourceConfig::default(),
                    };
                    let state = handle.state::<AppState>();
                    match commands::start_stream(handle.clone(), state, cfg) {
                        Ok(()) => eprintln!("[stream-test] start_stream 返回成功，进程未退出"),
                        Err(e) => eprintln!("[stream-test] start_stream 返回错误：{e}"),
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_stream,
            commands::stop_stream,
            commands::is_streaming,
            commands::preview_frame,
            commands::set_preview,
            commands::open_live_panel,
            commands::open_in_browser,
            commands::list_monitors,
            commands::list_windows,
            commands::set_audio_volume,
            commands::app_info,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}

fn env_logger_init() -> Result<(), log::SetLoggerError> {
    // 极简 logger：只输出到 stderr，不写文件、不上报、不联网
    struct Simple;
    impl log::Log for Simple {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            eprintln!(
                "[{}] {} {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
        fn flush(&self) {}
    }
    static LOGGER: Simple = Simple;
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info))
}

/// 把引擎事件转发到前端（WebView 侧 listen("engine-event")）。
pub fn forward_engine_events(
    app: tauri::AppHandle,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<lite_core::EngineEvent>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let _ = app.emit("engine-event", ev);
        }
    });
}
