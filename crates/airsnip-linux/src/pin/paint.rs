// SPDX-License-Identifier: GPL-3.0-only
//! 贴图绘制：每个输出一块整屏透明缓冲，只画「贴图 ∩ 本输出」那部分，
//! 并把输入区域限制在交集内（其余区域点击穿透）。
//!
//! `wl_shm` 的 `Argb8888` 是**直通 alpha**（非预乘），内存序 B,G,R,A。

use crate::wayland::Backend;

/// 边框色（与 Windows 版 `pin::border` 一致）。
const BORDER: (u8, u8, u8) = (0x1E, 0x90, 0xFF);

/// 两个矩形的交集（左, 上, 宽, 高）。
pub fn intersect(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> Option<(i32, i32, i32, i32)> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// 重绘所有输出的贴图表面。
pub fn paint_all(state: &mut Backend) {
    for i in 0..state.pins.len() {
        paint_pin(state, i);
    }
}

/// 重绘一块贴图涉及的所有输出表面。
pub fn paint_pin(state: &mut Backend, idx: usize) {
    let Some(pin) = state.pins.get_mut(idx) else {
        return;
    };
    let rect = pin.rect();
    let (zoom, opacity, border) = (pin.zoom, pin.opacity, pin.border);

    for li in 0..pin.layers.len() {
        let layer = &mut pin.layers[li];
        let Some(buf) = layer.buf.as_mut() else {
            continue;
        };
        // 缓冲忙（合成器还在用）就跳过这一帧：贴图单缓冲，宁丢一帧也不要撕裂
        if !buf.released.load(std::sync::atomic::Ordering::Acquire) {
            continue;
        }
        let origin = layer.origin;
        let size = layer.size;
        let inter = intersect(rect, (origin.0, origin.1, size.0, size.1));
        let (bw, bh) = (buf.w as u32, buf.h as u32);
        let px = buf.bytes_mut();
        px.fill(0);
        if let Some((ix, iy, iw, ih)) = inter {
            draw_image(
                px,
                bw,
                bh,
                &pin.image,
                (ix - origin.0, iy - origin.1),
                (iw, ih),
                (ix - rect.0, iy - rect.1),
                (rect.2, rect.3),
                zoom,
                opacity,
                border,
            );
        }

        // 输入区域 = 交集；没有交集就是空区域（整屏点击穿透）
        if let (Some(compositor), Some(qh)) = (state.compositor.clone(), state.qh.clone()) {
            let region = compositor.create_region(&qh, ());
            if let Some((ix, iy, iw, ih)) = inter {
                region.add(ix - origin.0, iy - origin.1, iw, ih);
            }
            layer.surface.set_input_region(Some(&region));
            region.destroy();
        }

        buf.released.store(false, std::sync::atomic::Ordering::Release);
        layer.surface.attach(Some(&buf.buffer), 0, 0);
        layer.surface.damage_buffer(0, 0, buf.w, buf.h);
        layer.surface.commit();
    }
}

/// 把贴图的一部分画进整屏缓冲。
///
/// - `at` / `size`：要画的目标矩形（**表面局部坐标**，= 交集在输出内的位置）；
/// - `src_at`：这块在贴图里的偏移（缩放后坐标系），用于取源像素与判断边框；
/// - `full`：贴图整体缩放后的尺寸（判断 1px 边框是否落在真实边缘）。
#[allow(clippy::too_many_arguments)]
pub fn draw_image(
    buf: &mut [u8],
    bw: u32,
    bh: u32,
    img: &crate::pin::image::PinImage,
    at: (i32, i32),
    size: (i32, i32),
    src_at: (i32, i32),
    full: (i32, i32),
    zoom: f32,
    opacity: u8,
    border: bool,
) {
    let opacity = opacity as u32;
    let (w, h) = size;
    for y in 0..h {
        let by = at.1 + y;
        if by < 0 || by >= bh as i32 {
            continue;
        }
        let fy = src_at.1 + y; // 贴图内的行（缩放后）
        let sy = ((fy as f32 / zoom) as u32).min(img.h.saturating_sub(1));
        for x in 0..w {
            let bx = at.0 + x;
            if bx < 0 || bx >= bw as i32 {
                continue;
            }
            let fx = src_at.0 + x;
            let sx = ((fx as f32 / zoom) as u32).min(img.w.saturating_sub(1));
            let si = ((sy * img.w + sx) * 4) as usize;
            let di = ((by as u32 * bw + bx as u32) * 4) as usize;
            let is_border = border && (fx == 0 || fy == 0 || fx == full.0 - 1 || fy == full.1 - 1);
            if is_border {
                buf[di] = BORDER.2;
                buf[di + 1] = BORDER.1;
                buf[di + 2] = BORDER.0;
                buf[di + 3] = opacity as u8;
            } else {
                buf[di] = img.px[si + 2];
                buf[di + 1] = img.px[si + 1];
                buf[di + 2] = img.px[si];
                buf[di + 3] = (img.px[si + 3] as u32 * opacity / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pin::image::PinImage;

    fn img(w: u32, h: u32) -> PinImage {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            px.extend_from_slice(&[i as u8, 10, 20, 255]);
        }
        PinImage { w, h, px }
    }

    fn blank(w: u32, h: u32) -> Vec<u8> {
        vec![0u8; (w * h * 4) as usize]
    }

    #[test]
    fn intersect_basic() {
        assert_eq!(
            intersect((100, 100, 50, 50), (0, 0, 1920, 1080)),
            Some((100, 100, 50, 50))
        );
        // 跨屏交界：贴图从 x=1900 开始、宽 100，落在左屏 (0..1920) 的部分只有 20px
        assert_eq!(
            intersect((1900, 10, 100, 40), (0, 0, 1920, 1080)),
            Some((1900, 10, 20, 40))
        );
        // 右屏上剩下的 80px
        assert_eq!(
            intersect((1900, 10, 100, 40), (1920, 0, 1920, 1080)),
            Some((1920, 10, 80, 40))
        );
        // 完全不在本屏
        assert_eq!(intersect((0, 0, 10, 10), (1920, 0, 1920, 1080)), None);
    }

    #[test]
    fn draws_slice_at_offset_in_bgra() {
        let src = img(4, 4);
        let mut buf = blank(8, 8);
        // 只画源图左上 2×2，放到缓冲 (3,4)
        draw_image(&mut buf, 8, 8, &src, (3, 4), (2, 2), (0, 0), (4, 4), 1.0, 255, false);
        let i = ((4 * 8 + 3) * 4) as usize;
        assert_eq!(&buf[i..i + 4], &[20, 10, 0, 255]);
        assert_eq!(&buf[0..4], &[0, 0, 0, 0], "贴图外保持透明");
    }

    #[test]
    fn border_only_on_real_edges() {
        let src = img(4, 4);
        let mut buf = blank(8, 8);
        // 只画右半边（src_at.x = 2）：左边界不该有边框，右边界该有
        draw_image(&mut buf, 8, 8, &src, (0, 0), (2, 4), (2, 0), (4, 4), 1.0, 255, true);
        let px = |x: u32, y: u32| {
            let i = ((y * 8 + x) * 4) as usize;
            (buf[i], buf[i + 1], buf[i + 2])
        };
        assert_ne!(px(0, 1), (0xFF, 0x90, 0x1E), "左边界不是贴图边缘，不该画边框");
        assert_eq!(px(1, 1), (0xFF, 0x90, 0x1E), "右边界应画边框");
    }

    #[test]
    fn opacity_scales_alpha() {
        let src = img(1, 1);
        let mut buf = blank(2, 2);
        draw_image(&mut buf, 2, 2, &src, (0, 0), (1, 1), (0, 0), (1, 1), 1.0, 128, false);
        assert_eq!(buf[3], 128);
    }

    #[test]
    fn zoom_out_samples_nearest() {
        let src = img(4, 4);
        let mut buf = blank(4, 4);
        draw_image(&mut buf, 4, 4, &src, (0, 0), (2, 2), (0, 0), (2, 2), 0.5, 255, false);
        let red = |x: u32, y: u32| buf[((y * 4 + x) * 4) as usize + 2];
        assert_eq!(red(0, 0), 0);
        assert_eq!(red(1, 0), 2);
        assert_eq!(red(0, 1), 8);
    }
}
