//! GPU 色彩转换自检：喂纯色 BGRA，读回 NV12，验证 YUV 数值是否合理，并测耗时。
//!
//! 运行：cargo run --release -p lite-core --example gpu_convert_test

use lite_core::encode::d3d::{gpu_canvas, BgraConverter};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

fn readback(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    w: u32,
    h: u32,
) -> Vec<u8> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
        let staging = staging.unwrap();
        context.CopyResource(&staging, tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .unwrap();
        let w = w as usize;
        let h = h as usize;
        let mut out = vec![0u8; w * h + w * (h / 2)];
        // 只 Map 子资源 0：NV12 的 UV 平面紧跟在 Y 平面之后（同一块分配），
        // 偏移 = RowPitch * height。这样避免对子资源 1 再 Map（部分驱动会拒绝）。
        for row in 0..h {
            let src = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(row * w), w);
        }
        let uv_base = (mapped.pData as *const u8).add(mapped.RowPitch as usize * h);
        for row in 0..h / 2 {
            let src = uv_base.add(row * mapped.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(w * h + row * w), w);
        }
        context.Unmap(&staging, 0);
        out
    }
}

fn fill(bgra: &mut [u8], w: u32, h: u32, b: u8, g: u8, r: u8) {
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 4;
            bgra[i] = b;
            bgra[i + 1] = g;
            bgra[i + 2] = r;
            bgra[i + 3] = 255;
        }
    }
}

fn main() {
    let w: u32 = std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(640);
    let h: u32 = std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(360);
    let canvas = gpu_canvas(w, h, "nvidia").expect("创建 GPU 画布失败");
    let conv = BgraConverter::new(&canvas.device, &canvas.context, &canvas.texture, w, h, w, h, 60)
        .expect("创建转换器失败");

    let stride = (w * 4) as usize;
    let mut frame = vec![0u8; stride * h as usize];

    for (name, b, g, r, y_lo, y_hi, u_lo, u_hi, v_lo, v_hi) in [
        ("纯红", 0u8, 0u8, 255u8, 70u16, 100u16, 80u16, 110u16, 220u16, 250u16),
        ("纯灰", 128, 128, 128, 110, 145, 115, 140, 115, 140),
        ("纯白", 255, 255, 255, 220, 255, 115, 140, 115, 140),
    ] {
        fill(&mut frame, w, h, b, g, r);
        conv.convert(&frame, stride).unwrap();
        let yuv = readback(&canvas.device, &canvas.context, &canvas.texture, w, h);
        let w_us = w as usize;
        let h_us = h as usize;
        let y: u32 = yuv[..w_us * h_us].iter().map(|v| *v as u32).sum::<u32>() / (w_us * h_us) as u32;
        let uv = &yuv[w_us * h_us..];
        let u: u32 = uv.iter().step_by(2).map(|v| *v as u32).sum::<u32>() / (uv.len() / 2) as u32;
        let v: u32 = uv.iter().skip(1).step_by(2).map(|v| *v as u32).sum::<u32>() / (uv.len() / 2) as u32;
        let ok = (y as u16 >= y_lo && y as u16 <= y_hi)
            && (u as u16 >= u_lo && u as u16 <= u_hi)
            && (v as u16 >= v_lo && v as u16 <= v_hi);
        println!("  {name}: Y={y} U={u} V={v} → {}", if ok { "通过" } else { "超出预期" });
    }

    // 耗时（含一次内存上传 + GPU 转换）
    let n = 200;
    let t = std::time::Instant::now();
    for _ in 0..n {
        conv.convert(&frame, stride).unwrap();
    }
    let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("  {w}x{h} 转换 {per:.3} ms/帧（含一次 CPU 行内存上传）");
}
