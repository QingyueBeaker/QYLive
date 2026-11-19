//! 量一下 CPU 侧 BGRA->NV12 转换与缩放的实测成本（决定 GPU 转换值不值得做）。
use std::time::Instant;

fn bench(label: &str, n: usize, mut f: impl FnMut()) {
    for _ in 0..5 {
        f();
    }
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    println!("{label}: {:.2} ms/帧", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
}

fn main() {
    // 1080p：转换
    let (w, h) = (1920u32, 1080u32);
    let stride = (w * 4) as usize;
    let src = vec![120u8; stride * h as usize];
    let mut dst = Vec::new();
    bench("1920x1080 BGRA->NV12（纯转换）", 100, || {
        lite_core::convert::bgra_to_nv12(&src, stride, w, h, &mut dst);
    });

    // 用户实际链路：2560x1440 采集 -> 1920x1080 推流（先缩放再转换）
    let (sw, sh) = (2560u32, 1440u32);
    let sstride = (sw * 4) as usize;
    let big = vec![120u8; sstride * sh as usize];
    let mut scaled = Vec::new();
    bench("2560x1440 -> 1920x1080 BGRA 缩放（CPU）", 50, || {
        lite_core::convert::bgra_scale_to_bgra(&big, sstride, sw, sh, w, h, &mut scaled);
    });
    bench("  缩放后 1080p 再转 NV12", 50, || {
        lite_core::convert::bgra_to_nv12(&scaled, (w * 4) as usize, w, h, &mut dst);
    });
}
