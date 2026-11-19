//! 前端可调用的命令集合。

use std::sync::atomic::Ordering;

use lite_core::{EngineEvent, PreviewState, RunConfig};
use serde::Serialize;
use tauri::ipc::Response;
use tauri::State;

use crate::{forward_engine_events, AppState};

#[derive(Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub platform: String,
}

#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: "Lite RTMP Streamer".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        platform: "Windows 10/11 x64".into(),
    }
}

#[tauri::command]
pub fn start_stream(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    cfg: RunConfig,
) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|_| "状态锁异常".to_string())?;
    // 已有会话则先干净停止：允许“重试连接”直接重启，而不是把用户挡在“正在推流中”外面
    if let Some(mut s) = guard.take() {
        s.stop();
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    forward_engine_events(app.clone(), rx);
    // 关键：同步命令线程里没有 tokio 运行时，session::start 内部会 tokio::spawn，
    // 必须进入 Tauri 的异步运行时上下文，否则 panic（release 下 panic=abort 会直接闪退）。
    let preview = state.preview.clone();
    let session = tauri::async_runtime::block_on(async move {
        lite_core::start(cfg, tx, preview)
    })?;
    *guard = Some(session);
    Ok(())
}

#[tauri::command]
pub fn stop_stream(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|_| "状态锁异常".to_string())?;
    if let Some(mut s) = guard.take() {
        s.stop();
    }
    Ok(())
}

#[tauri::command]
pub fn is_streaming(state: State<'_, AppState>) -> bool {
    state
        .session
        .lock()
        .map(|g| g.as_ref().map(|s| !s.is_finished()).unwrap_or(false))
        .unwrap_or(false)
}

/// 打开/关闭预览分支。关闭时引擎侧完全不复制、不缩放、不渲染。
#[tauri::command]
pub fn set_preview(
    _app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
    max_width: Option<u32>,
    fps: Option<u32>,
) -> Result<(), String> {
    let preview = state.preview.clone();
    if let Some(w) = max_width {
        preview.max_width.store(w.max(64), Ordering::Relaxed);
    }
    if let Some(f) = fps {
        preview.fps_limit.store(f.max(1).min(60), Ordering::Relaxed);
    }
    preview.set_enabled(enabled);
    // 预览改为在主窗口内渲染：这里只切换引擎侧的克隆开关，不再创建任何额外窗口
    Ok(())
}

/// 预览帧：二进制返回（前 12 字节为 seq/宽/高，其后为 RGBA 像素）。
#[tauri::command]
pub fn preview_frame(state: State<'_, AppState>) -> Response {
    let (seq, w, h, data) = state.preview.take();
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.extend_from_slice(&data);
    Response::new(out)
}

/// 打开主播后台：直接用系统浏览器打开官方页面。
/// 说明：不再用内嵌 WebView 窗口——多 WebView 窗口在部分机器上会阻塞主事件循环，
/// 表现为“窗口打不开 / 关不掉 / 界面无响应”。系统浏览器同样满足“不注入、不抓取”的合规要求。
#[tauri::command]
pub fn open_live_panel() -> Result<(), String> {
    // 直播中心：开播、分区/标题设置、以及平台自己的“开始直播”按钮都在这里
    open_in_browser("https://link.bilibili.com/p/center/index".to_string())
}

/// 用系统默认浏览器打开指定地址（仅打开链接，不注入、不抓取任何数据）。
#[tauri::command]
pub fn open_in_browser(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 http/https 链接".into());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {e}"))?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[tauri::command]
pub fn list_monitors() -> Vec<MonitorInfo> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };

    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return out,
        };
        let mut index = 0usize;
        let mut adapter_idx = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            adapter_idx += 1;
            let mut out_idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(out_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                out_idx += 1;
                if let Ok(desc) = output.GetDesc() {
                    let rc = desc.DesktopCoordinates;
                    let mut mi = MONITORINFOEXW::default();
                    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
                    let hmon: HMONITOR = desc.Monitor;
                    let primary = GetMonitorInfoW(hmon, &mut mi.monitorInfo as *mut MONITORINFO)
                        .as_bool()
                        && (mi.monitorInfo.dwFlags & 1) != 0;
                    let name = String::from_utf16_lossy(
                        &mi.szDevice[..mi
                            .szDevice
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(mi.szDevice.len())],
                    );
                    out.push(MonitorInfo {
                        index,
                        name,
                        width: (rc.right - rc.left).max(0) as u32,
                        height: (rc.bottom - rc.top).max(0) as u32,
                        primary,
                    });
                    index += 1;
                }
            }
        }
    }
    out
}

#[derive(Serialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
}

#[tauri::command]
pub fn list_windows() -> Vec<WindowInfo> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<WindowInfo>);
        if IsWindowVisible(hwnd).as_bool() {
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(hwnd, &mut buf);
                if n > 0 {
                    let title = String::from_utf16_lossy(&buf[..n as usize]);
                    if !title.trim().is_empty() {
                        list.push(WindowInfo {
                            hwnd: hwnd.0 as isize,
                            title,
                        });
                    }
                }
            }
        }
        BOOL(1)
    }

    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out.truncate(200);
    out
}

/// 实时音量（0.0 - 1.0）。未开播时只记录，下次开播生效。
#[tauri::command]
pub fn set_audio_volume(state: State<'_, AppState>, which: String, gain: f32) -> Result<(), String> {
    let g = gain.clamp(0.0, 2.0);
    let guard = state.session.lock().map_err(|_| "状态锁异常".to_string())?;
    if let Some(s) = guard.as_ref() {
        use std::sync::atomic::Ordering as O;
        if which == "system" {
            s.volumes
                .system
                .store((g * 1000.0) as u32, O::Relaxed);
        } else {
            s.volumes.mic.store((g * 1000.0) as u32, O::Relaxed);
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _preview_type_hint(_: &PreviewState) {}
