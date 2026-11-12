// SPDX-License-Identifier: GPL-3.0-only
//! 贴图绘制：最近邻缩放 + 透明度 + 1px 蓝框，直接写进 `wl_shm` 缓冲。
//!
//! 缩放与透明度在这里做而不是靠合成器：纯 CPU、即时生效，也不用依赖任何特效协议。
//! `wl_shm` 的 `Argb8888` 是**直通 alpha**（非预乘），内存序 B,G,R,A。

use crate::wayland::Backend;

/// 边框色（与 Windows 版 `pin::border` 一致）。
const BORDER: (u8, u8, u8) = (0x1E, 0x90, 0xFF);

/// 重绘所有贴图。
pub fn paint_all(state: &mut Backend) {
    for i in 0..state.pins.len() {
        paint_pin(state, i);
    }
}

/// 重绘一块贴图。
pub fn paint_pin(state: &mut Backend, idx: usize) {
    let Some(pin) = state.pins.get_mut(idx) else {
        return;
    };
    if pin.bufs.is_empty() {
        return;
    }
    let Some(bi) = pin.take_buffer() else {
        return;
    };
    {
        let (zoom, opacity, border) = (pin.zoom, pin.opacity, pin.border);
        let img = &pin.image;
        let buf = &mut pin.bufs[bi];
        let (w, h) = (buf.w as u32, buf.h as u32);
        let rendered = render(img, w, h, zoom, opacity, border);
        buf.bytes_mut()[..rendered.len()].copy_from_slice(&rendered);
    }
    pin.present(&pin.bufs[bi]);
}

/// 把源图渲染成 `out_w × out_h` 的 BGRA（直通 alpha）：最近邻缩放 + 透明度 + 1px 蓝框。
///
/// 抽成纯函数是为了能单测：像素顺序、边框颜色、alpha 缩放都能直接断言。
pub fn render(
    img: &crate::pin::image::PinImage,
    out_w: u32,
    out_h: u32,
    zoom: f32,
    opacity: u8,
    border: bool,
) -> Vec<u8> {
    let mut px = vec![0u8; (out_w * out_h * 4) as usize];
    {
        let opacity = opacity as u32;
        let (w, h) = (out_w, out_h);
        // 最近邻缩放：目标像素反算回源坐标
        for y in 0..h {
            let sy = ((y as f32 / zoom) as u32).min(img.h.saturating_sub(1));
            for x in 0..w {
                let sx = ((x as f32 / zoom) as u32).min(img.w.saturating_sub(1));
                let si = ((sy * img.w + sx) * 4) as usize;
                let di = ((y * w + x) * 4) as usize;
                let a = img.px[si + 3] as u32 * opacity / 255;
                px[di] = img.px[si + 2];
                px[di + 1] = img.px[si + 1];
                px[di + 2] = img.px[si];
                px[di + 3] = a as u8;
            }
        }
        // 1px 蓝框（含 alpha，随透明度一起变淡）
        if border {
            let a = opacity as u8;
            let edge = |px: &mut [u8], off: usize| {
                px[off] = BORDER.2;
                px[off + 1] = BORDER.1;
                px[off + 2] = BORDER.0;
                px[off + 3] = a;
            };
            for x in 0..w {
                let top = (x * 4) as usize;
                let bottom = (((h - 1) * w + x) * 4) as usize;
                edge(&mut px, top);
                edge(&mut px, bottom);
            }
            for y in 0..h {
                let left = ((y * w) * 4) as usize;
                let right = ((y * w + (w - 1)) * 4) as usize;
                edge(&mut px, left);
                edge(&mut px, right);
            }
        }
    }
    px
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

    #[test]
    fn copies_pixels_in_bgra_order() {
        let src = img(2, 2);
        let out = render(&src, 2, 2, 1.0, 255, false);
        // 源 RGBA = (0,10,20,255) → 缓冲应是 B,G,R,A
        assert_eq!(&out[0..4], &[20, 10, 0, 255]);
    }

    #[test]
    fn opacity_scales_alpha() {
        let src = img(1, 1);
        let out = render(&src, 1, 1, 1.0, 128, false);
        assert_eq!(out[3], 128);
    }

    #[test]
    fn border_paints_edges_blue() {
        let src = img(4, 4);
        let out = render(&src, 4, 4, 1.0, 255, true);
        // 左上角像素应被边框覆盖（BGR = FF 90 1E）
        assert_eq!(&out[0..3], &[0xFF, 0x90, 0x1E]);
        // 内部像素保持原色
        let i = ((1 * 4 + 1) * 4) as usize;
        assert_eq!(&out[i..i + 3], &[20, 10, 5]);
    }

    #[test]
    fn zoom_out_samples_nearest() {
        let src = img(4, 4);
        // 缩到 2×2：每个目标像素取源图 (2x, 2y)
        let out = render(&src, 2, 2, 0.5, 255, false);
        let at = |x: usize, y: usize| out[(y * 2 + x) * 4 + 2]; // R 通道 = 源索引
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(1, 0), 2);
        assert_eq!(at(0, 1), 8);
    }
}
