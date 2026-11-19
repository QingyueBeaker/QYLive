//! GPU 编码路径实测：分别用「GPU 纹理直通」与「纯 CPU 内存」编码合成 NV12 帧，
//! 打印选中的编码器、成功帧数与耗时，用于确认硬件编码是否真的跑起来了。
//!
//! 运行：cargo run --release -p lite-core --example gpu_encode_test

use lite_core::config::{RateControl, VideoConfig};
use lite_core::encode::h264::MfH264Encoder;

fn frame(width: u32, height: u32, tick: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![0u8; w * h + w * (h / 2)];
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = (((x as u32 + y as u32 + tick) & 0xFF) as u8).max(16);
        }
    }
    for i in 0..(w * (h / 2)) {
        buf[w * h + i] = 128;
    }
    buf
}

fn run(prefer_gpu: bool) {
    let cfg = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 8000,
        gop_seconds: 2,
        rate_control: RateControl::Abr,
        prefer_gpu,
    };
    println!(
        "\n=== prefer_gpu={prefer_gpu} {}x{}@{} {}kbps ABR",
        cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps
    );
    let mut enc = match MfH264Encoder::new(&cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("  创建失败: {e}");
            return;
        }
    };
    println!("  选中编码器: {}（硬件={}）", enc.info.name, enc.info.hardware);

    let mut ok = 0u32;
    let mut total_bytes = 0usize;
    let mut keyframes = 0u32;
    let start = std::time::Instant::now();
    for i in 0..90u32 {
        let f = frame(cfg.width, cfg.height, i * 3);
        let pts = (i as i64) * (10_000_000 / cfg.fps as i64);
        match enc.encode(&f, pts) {
            Ok(frames) => {
                for fr in &frames {
                    total_bytes += fr.data.len();
                    if fr.keyframe {
                        keyframes += 1;
                    }
                }
                if i == 0 && enc.extradata().is_none() {
                    println!("  [警告] 首帧后仍无 extradata");
                }
                ok += 1;
            }
            Err(e) => {
                println!("  第 {i} 帧失败: {e}");
                break;
            }
        }
    }
    let ms = start.elapsed().as_millis();
    let fps = if ms > 0 { ok as f64 * 1000.0 / ms as f64 } else { 0.0 };
    println!(
        "  成功 {ok}/90 帧，{:.0} fps，关键帧 {keyframes}，输出 {:.0} KB，extradata={}",
        fps,
        total_bytes as f64 / 1024.0,
        enc.extradata().is_some()
    );
}

fn main() {
    unsafe {
        let _ = windows::Win32::Media::MediaFoundation::MFStartup(
            windows::Win32::Media::MediaFoundation::MF_VERSION,
            windows::Win32::Media::MediaFoundation::MFSTARTUP_LITE,
        );
    }
    // LITE_HW=1：允许硬件编码器参与（不使用 D3D11 纹理，走系统内存输入）
    // LITE_GPU=1：额外启用 D3D11 纹理直通
    let prefer_gpu = std::env::var_os("LITE_HW").is_some() || std::env::var_os("LITE_GPU").is_some();
    run(prefer_gpu);
}
