// SPDX-License-Identifier: GPL-3.0-only
//! 贴图源图的读取（PNG → RGBA8888）。

use std::path::Path;

use crate::Error;

/// 一张待贴的位图。
pub struct PinImage {
    pub w: u32,
    pub h: u32,
    /// RGBA8888，行优先，无行尾填充。
    pub px: Vec<u8>,
}

/// 读 PNG（截图导出就是这么一份文件）。
pub fn load_png(path: &Path) -> Result<PinImage, Error> {
    let file = std::fs::File::open(path).map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info()?;
    let size = reader.output_buffer_size().ok_or("PNG 尺寸过大")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width, info.height);
    // 统一成 RGBA8888（截图是 RGBA，但外部 PNG 可能是 RGB / 灰度）
    let px = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf[..info.buffer_size()]
            .iter()
            .flat_map(|g| [*g, *g, *g, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..info.buffer_size()]
            .chunks_exact(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        other => return Err(format!("不支持的 PNG 颜色类型：{other:?}").into()),
    };
    if px.len() < (w * h * 4) as usize {
        return Err("PNG 数据长度不足".into());
    }
    Ok(PinImage { w, h, px })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_png() {
        // 造一张 4×3 的图，写 PNG 再读回来
        let mut canvas = crate::canvas::Canvas::new(4, 3);
        for (i, px) in canvas.px.chunks_exact_mut(4).enumerate() {
            px[0] = i as u8;
            px[1] = 50;
            px[2] = 200;
            px[3] = 255;
        }
        let dir = std::env::temp_dir().join(format!("airsnip-pin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        crate::codec::write_png(&canvas, &path).unwrap();
        let img = load_png(&path).unwrap();
        assert_eq!((img.w, img.h), (4, 3));
        assert_eq!(&img.px[4..8], &[1, 50, 200, 255]);
        std::fs::remove_file(&path).ok();
    }
}
