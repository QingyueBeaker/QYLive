//! NVENC 直连实测：打印创建失败的真实原因，并测吞吐。
//!
//! 运行：cargo run --release -p lite-core --example nvenc_test
//! 可选环境变量：W / H / FPS / KBPS

use lite_core::config::{RateControl, VideoConfig};
use lite_core::encode::nvenc::{NvencH264Encoder, VideoEncoder};

fn frame(width: u32, height: u32, tick: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![0u8; w * h + w * (h / 2)];
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = (((x as u32 ^ y as u32).wrapping_add(tick * 7)) & 0xFF) as u8;
        }
    }
    buf[w * h..].fill(128);
    buf
}

fn run(enc: &mut VideoEncoder, w: u32, h: u32, fps: u32) {
    println!("  选中编码器: {}（硬件={}）", enc.name(), enc.is_hardware());
    let frames = 120u32;
    let mut ok = 0u32;
    let mut bytes_total = 0usize;
    let mut keys = 0u32;
    let start = std::time::Instant::now();
    for i in 0..frames {
        let f = frame(w, h, i);
        let pts = (i as i64) * (10_000_000 / fps as i64);
        match enc.encode(&f, pts) {
            Ok(out) => {
                for e in &out {
                    bytes_total += e.data.len();
                    if e.keyframe {
                        keys += 1;
                    }
                }
                ok += 1;
            }
            Err(e) => {
                println!("  第 {i} 帧失败: {e}");
                break;
            }
        }
    }
    let ms = start.elapsed().as_secs_f64().max(0.001);
    println!(
        "  成功 {ok}/{frames} 帧，{:.0} fps，关键帧 {keys}，平均 {:.0} kbps，extradata={}",
        ok as f64 / ms,
        (bytes_total as f64 * 8.0 / 1000.0) / ms,
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
    let w: u32 = std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: u32 = std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(1080);
    let fps: u32 = std::env::var("FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let kbps: u32 = std::env::var("KBPS").ok().and_then(|v| v.parse().ok()).unwrap_or(8000);

    let rate_control = match std::env::var("RC").unwrap_or_default().to_ascii_lowercase().as_str() {
        "cbr" => RateControl::Cbr,
        "vbr" => RateControl::Vbr,
        _ => RateControl::Abr,
    };
    let cfg = VideoConfig {
        width: w,
        height: h,
        fps,
        bitrate_kbps: kbps,
        gop_seconds: 2,
        rate_control,
        prefer_gpu: false,
        prefer_nvenc: true,
    };
    println!("=== {w}x{h}@{fps} {kbps}kbps ABR");

    match NvencH264Encoder::new(&cfg) {
        Ok(e) => {
            let mut enc = VideoEncoder::Nvenc(e);
            run(&mut enc, w, h, fps);
        }
        Err(err) => {
            println!("  NVENC 创建失败: {err}");
            // 再试引擎的自动回退路径，确认软件编码兜底正常
            match VideoEncoder::new(&cfg) {
                Ok(mut e) => {
                    println!("  已回退: {}", e.name());
                    run(&mut e, w, h, fps);
                }
                Err(e2) => println!("  回退也失败: {e2}"),
            }
        }
    }
}
