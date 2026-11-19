//! 像素格式转换与缩放。
//!
//! 采集层统一输出 BGRA（32bpp）。编码器（Media Foundation H.264）需要 NV12。
//! 这里用整数 BT.601（limited range）公式做手写转换，避免额外挂一个
//! MFT 视频处理器带来的设备创建开销与失败面。

/// BGRA -> NV12（Y 平面 + 交错 UV 平面），尺寸要求为偶数。
pub fn bgra_to_nv12(src: &[u8], stride: usize, width: u32, height: u32, dst: &mut Vec<u8>) {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let y_size = w * h;
    let uv_size = w * h / 2;
    dst.resize(y_size + uv_size, 0);

    let (y_plane, uv_plane) = dst.split_at_mut(y_size);

    // Y 平面
    for row in 0..h {
        let src_row = &src[row * stride..];
        let y_row = &mut y_plane[row * w..row * w + w];
        for col in 0..w {
            let p = &src_row[col * 4..col * 4 + 3];
            let b = p[0] as i32;
            let g = p[1] as i32;
            let r = p[2] as i32;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_row[col] = y.clamp(0, 255) as u8;
        }
    }

    // UV 平面：2x2 下采样后再做色度转换
    for row in (0..h).step_by(2) {
        let s0 = &src[row * stride..];
        let s1 = &src[(row + 1) * stride..];
        let uv_row = &mut uv_plane[(row / 2) * w..(row / 2) * w + w];
        for col in (0..w).step_by(2) {
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for (s, c) in [(s0, col), (s0, col + 1), (s1, col), (s1, col + 1)] {
                let p = &s[c * 4..c * 4 + 3];
                b += p[0] as i32;
                g += p[1] as i32;
                r += p[2] as i32;
            }
            r >>= 2;
            g >>= 2;
            b >>= 2;
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            uv_row[col] = u.clamp(0, 255) as u8;
            uv_row[col + 1] = v.clamp(0, 255) as u8;
        }
    }
}

/// BGRA -> RGBA 的等比缩小（最近采样），用于预览分支，成本极低。
/// 关闭预览时此函数完全不会被调用。
pub fn bgra_scale_to_rgba(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    dst_w: u32,
    dst_h: u32,
    dst: &mut Vec<u8>,
) {
    dst.clear();
    dst.reserve((dst_w as usize) * (dst_h as usize) * 4);
    let step_x = (width as f32 / dst_w as f32).max(1.0);
    let step_y = (height as f32 / dst_h as f32).max(1.0);
    for y in 0..dst_h {
        let sy = ((y as f32 * step_y) as usize).min(height as usize - 1);
        let src_row = &src[sy * stride..];
        for x in 0..dst_w {
            let sx = ((x as f32 * step_x) as usize).min(width as usize - 1);
            let p = &src_row[sx * 4..sx * 4 + 4];
            dst.push(p[2]);
            dst.push(p[1]);
            dst.push(p[0]);
            dst.push(255);
        }
    }
}

/// BGRA -> BGRA 等比缩小/放大（最近采样）。用于把屏幕分辨率对齐到推流分辨率。
pub fn bgra_scale_to_bgra(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    dst_w: u32,
    dst_h: u32,
    dst: &mut Vec<u8>,
) {
    dst.clear();
    dst.reserve((dst_w as usize) * (dst_h as usize) * 4);
    let step_x = (width as f32 / dst_w as f32).max(1.0);
    let step_y = (height as f32 / dst_h as f32).max(1.0);
    for y in 0..dst_h {
        let sy = ((y as f32 * step_y) as usize).min(height as usize - 1);
        let src_row = &src[sy * stride..];
        for x in 0..dst_w {
            let sx = ((x as f32 * step_x) as usize).min(width as usize - 1);
            let p = &src_row[sx * 4..sx * 4 + 4];
            dst.extend_from_slice(p);
        }
    }
}

/// 生成一帧纯黑 NV12，用于采集切换间隙填补，保证时间戳与帧率不断流。
pub fn black_nv12(width: u32, height: u32, dst: &mut Vec<u8>) {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    dst.resize(w * h * 3 / 2, 0);
    let y_size = w * h;
    for v in dst[..y_size].iter_mut() {
        *v = 16;
    }
    for v in dst[y_size..].iter_mut() {
        *v = 128;
    }
}
