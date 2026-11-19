//! 自检程序：验证本机上的采集 / 像素转换 / H.264 / AAC 是否真实可用。
//!
//! 运行方式：
//!   cargo run -p lite-core --example selftest
//!
//! 注意：该示例只做本地计算，不联网、不推流、不写文件。

use std::time::Instant;

use lite_core::capture::create_backend;
use lite_core::config::{CaptureBackendKind, VideoConfig};
use lite_core::encode::aac::MfAacEncoder;
use lite_core::encode::h264::MfH264Encoder;

fn main() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    println!("== Lite RTMP Streamer 自检 ==");

    let backend_kind = CaptureBackendKind::Dxgi;
    let mut cap = create_backend(backend_kind, 0, 0);
    match cap.start() {
        Ok(()) => println!("[采集] {} 启动成功：{}", backend_kind.label(), cap.describe()),
        Err(e) => {
            println!("[采集] DXGI 启动失败（{e}），退回 GDI");
            let _ = cap.stop();
            cap = create_backend(CaptureBackendKind::Gdi, 0, 0);
            if let Err(e2) = cap.start() {
                println!("[采集] GDI 也失败：{e2}");
                return;
            }
        }
    }

    // 取第 1 帧：确认画面与尺寸
    let first = match cap.get_frame(1000) {
        Ok(Some(f)) => f,
        _ => {
            println!("[采集] 1 秒内没有拿到帧");
            return;
        }
    };
    println!(
        "[采集] 首帧 {}x{} stride={} 平均亮度={}",
        first.width,
        first.height,
        first.stride,
        avg_luma(&first.data)
    );

    // 连续取 30 帧，测采集吞吐
    let t0 = Instant::now();
    let mut frames = 0u32;
    let mut black = 0u32;
    while frames < 30 {
        if let Ok(Some(f)) = cap.get_frame(30) {
            if f.is_black() {
                black += 1;
            }
            frames += 1;
        }
        if t0.elapsed().as_secs() > 5 {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "[采集] {frames} 帧 / {:.2}s = {:.1} fps（其中疑似黑屏 {} 帧）",
        secs,
        frames as f64 / secs,
        black
    );
    let _ = cap.stop();

    // H.264：把采集帧缩放到 1280x720 后编码
    println!("[编码] 已枚举的编码器：{:?}", lite_core::encode::h264::describe_candidates());
    let vcfg = VideoConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_kbps: 4000,
        gop_seconds: 2,
        rate_control: lite_core::config::RateControl::Cbr,
    };
    let mut scaled = Vec::new();
    lite_core::convert::bgra_scale_to_bgra(
        &first.data,
        first.stride,
        first.width,
        first.height,
        vcfg.width,
        vcfg.height,
        &mut scaled,
    );
    let mut nv12 = Vec::new();
    lite_core::convert::bgra_to_nv12(
        &scaled,
        (vcfg.width * 4) as usize,
        vcfg.width,
        vcfg.height,
        &mut nv12,
    );
    println!("[转换] BGRA -> NV12 输出 {} 字节（期望 {}）", nv12.len(), vcfg.width as usize * vcfg.height as usize * 3 / 2);

    match MfH264Encoder::new(&vcfg) {
        Ok(mut enc) => {
            println!(
                "[编码] H.264 编码器：{}（{}）",
                enc.info.name,
                if enc.info.hardware { "硬件" } else { "软件" }
            );
            println!(
                "[编码] 序列头 avcC: {}",
                enc.extradata().map(|e| e.len()).unwrap_or(0)
            );
            let t1 = Instant::now();
            let mut total = 0usize;
            let mut keyframes = 0usize;
            let mut errors = 0usize;
            for i in 0..60 {
                if i == 30 {
                    enc.request_keyframe();
                }
                match enc.encode(&nv12, (i as i64) * 166_666) {
                    Ok(frames) => {
                        for f in frames {
                            total += f.data.len();
                            if f.keyframe {
                                keyframes += 1;
                            }
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if errors < 3 {
                            println!("[编码] 编码失败：{e}");
                        }
                    }
                }
            }
            let dt = t1.elapsed().as_secs_f64();
            println!(
                "[编码] 60 帧耗时 {:.0} ms（{:.1} fps），输出 {} 字节，关键帧 {}，失败 {}",
                dt * 1000.0,
                60.0 / dt,
                total,
                keyframes,
                errors
            );
            let _ = enc.finish();
        }
        Err(e) => println!("[编码] H.264 初始化失败：{e}"),
    }

    // AAC：512 帧正弦波
    match MfAacEncoder::new(128) {
        Ok(mut enc) => {
            let asc = enc.asc();
            println!("[音频] AAC 编码器就绪，ASC = {:02X?}", asc);
            let t2 = Instant::now();
            let mut total = 0usize;
            let mut count = 0usize;
            for n in 0..100 {
                let mut block = Vec::with_capacity(2048);
                for i in 0..1024 {
                    let t = (n * 1024 + i) as f64 / 48_000.0;
                    let v = (t * 440.0 * std::f64::consts::TAU).sin() * 0.35;
                    let s = (v * 32767.0) as i16;
                    block.push(s);
                    block.push(s);
                }
                match enc.encode(&block) {
                    Ok(packets) => {
                        for p in packets {
                            total += p.len();
                            count += 1;
                        }
                    }
                    Err(e) => {
                        println!("[音频] 编码失败：{e}");
                        break;
                    }
                }
            }
            let dt = t2.elapsed().as_secs_f64();
            println!(
                "[音频] 100 个 AAC 帧 -> {} 包，共 {} 字节，耗时 {:.0} ms（{:.1} fps）",
                count,
                total,
                dt * 1000.0,
                100.0 / dt
            );
        }
        Err(e) => println!("[音频] AAC 初始化失败：{e}"),
    }

    // RTMP 地址解析
    match lite_core::RunConfig::split_url("rtmp://live-push.example.com/live-bvc/?streamname=abc") {
        Ok((host, app)) => println!("[RTMP] URL 解析 -> host={host} app={app}"),
        Err(e) => println!("[RTMP] URL 解析失败：{e}"),
    }

    println!("== 自检结束 ==");
    unsafe {
        windows::Win32::System::Com::CoUninitialize();
    }
}

fn avg_luma(bgra: &[u8]) -> u32 {
    if bgra.len() < 4 {
        return 0;
    }
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    let step = (bgra.len() / 2048).max(4);
    let mut i = 0;
    while i + 3 < bgra.len() {
        sum += bgra[i + 2] as u64;
        n += 1;
        i += step;
    }
    if n == 0 {
        0
    } else {
        (sum / n) as u32
    }
}
