// SPDX-License-Identifier: GPL-3.0-only
//! Linux 侧文本引擎：cosmic-text（Harfbuzz 级整形 + Swash 栅格化）。
//!
//! 接进 core 的 `TextSink` 接口（ADR 0034），于是 `draw_objects` 这套绘制代码
//! 两个前端共用一份。字体查找走 fontconfig（`FontSystem::new()`），中文由
//! 系统回退链解决（思源黑体 / Noto CJK）。
//!
//! `TextSink` 的方法签名是 `&self`，而 cosmic-text 需要 `&mut FontSystem`，
//! 所以内部用 `RefCell` 包一层；UI 是单线程的，不存在跨线程争用。

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style as TextStyle, SwashCache, Weight};
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

use airsnip_core::draw::text::{TextSink, TextSpec};
use airsnip_core::editor::Point;

struct Inner {
    font_system: FontSystem,
    cache: SwashCache,
}

/// cosmic-text 文本引擎。
pub struct CosmicText {
    inner: RefCell<Inner>,
}

impl CosmicText {
    fn new() -> Self {
        Self {
            inner: RefCell::new(Inner {
                font_system: FontSystem::new(),
                cache: SwashCache::new(),
            }),
        }
    }

    fn attrs(spec: &TextSpec) -> Attrs<'_> {
        let family = if spec.family.is_empty() {
            Family::SansSerif
        } else {
            Family::Name(&spec.family)
        };
        Attrs::new()
            .family(family)
            .weight(if spec.bold { Weight::BOLD } else { Weight::NORMAL })
            .style(if spec.italic {
                TextStyle::Italic
            } else {
                TextStyle::Normal
            })
    }

    /// 整形一版文本；`width` 为 Some 时按该宽度软换行。
    fn shape(text: &str, spec: &TextSpec, inner: &mut Inner, width: Option<f32>) -> Buffer {
        let metrics = Metrics::new(spec.size, spec.size * 1.25);
        let mut buffer = Buffer::new(&mut inner.font_system, metrics);
        buffer.set_size(width, None);
        buffer.set_text(text, &Self::attrs(spec), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut inner.font_system, false);
        buffer
    }
}

/// 全局单例：`TextSink` 的方法签名是 `&self`，绘制是单线程 UI 流程，
/// 用 `Mutex` 包一层即可满足 `Sync`（不会嵌套加锁：只在最外层调用处取一次）。
pub fn with_engine<R>(f: impl FnOnce(&CosmicText) -> R) -> R {
    static ENGINE: OnceLock<Mutex<CosmicText>> = OnceLock::new();
    let m = ENGINE.get_or_init(|| Mutex::new(CosmicText::new()));
    let guard = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// 字形墨迹的包围盒（相对绘制原点 `(0,0)`）。
///
/// 直接在**字形图像**的 placement 上取并集，而不是用行高：cosmic-text 的
/// `line_height` 含 extra leading，比墨迹高得多，用它做居中会把数字推下去
/// （Windows 的 DirectWrite 度量本来就更贴近墨迹，所以两边看起来不一致）。
fn ink_bounds(inner: &mut Inner, text: &str, spec: &TextSpec) -> (f32, f32, f32, f32) {
    let buffer = CosmicText::shape(text, spec, inner, None);
    let mut min = (f32::MAX, f32::MAX);
    let mut max = (f32::MIN, f32::MIN);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, run.line_y), 1.0);
            if let Some(image) = inner.cache.get_image(&mut inner.font_system, physical.cache_key) {
                let left = (physical.x + image.placement.left) as f32;
                let top = (physical.y - image.placement.top) as f32;
                let right = left + image.placement.width as f32;
                let bottom = top + image.placement.height as f32;
                min.0 = min.0.min(left);
                min.1 = min.1.min(top);
                max.0 = max.0.max(right);
                max.1 = max.1.max(bottom);
            }
        }
    }
    if min.0 > max.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (min.0, min.1, max.0, max.1)
}

impl TextSink for CosmicText {
    /// 返回**墨迹尺寸**（宽, 高），保证 `draw(top_left)` 画出的字形正好落在这个框里。
    fn measure(&self, text: &str, spec: &TextSpec) -> (f32, f32) {
        let mut inner = self.inner.borrow_mut();
        let (min_x, min_y, max_x, max_y) = ink_bounds(&mut inner, text, spec);
        ((max_x - min_x).max(0.0), (max_y - min_y).max(0.0))
    }

    fn draw(&self, pixmap: &mut Pixmap, pos: Point, text: &str, spec: &TextSpec, color: [u8; 4], tr: Transform) {
        let mut inner = self.inner.borrow_mut();
        let buffer = CosmicText::shape(text, spec, &mut inner, None);
        // 墨迹相对原点的偏移：减掉它，字形左上角就正好落在 pos
        let (ink_x, ink_y, _, _) = ink_bounds(&mut inner, text, spec);
        let Inner { font_system, cache } = &mut *inner;
        let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
        let base = cosmic_text::Color::rgba(color[0], color[1], color[2], color[3]);

        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((pos.x - ink_x, pos.y + run.line_y - ink_y), 1.0);
                // tiny-skia 的 map_point 是「传入可变点就地变换」
                let mut origin = tiny_skia::Point { x: 0.0, y: 0.0 };
                tr.map_point(&mut origin);
                let (dx, dy) = (origin.x.round() as i32, origin.y.round() as i32);
                let pixels = pixmap.pixels_mut();
                cache.with_pixels(font_system, physical.cache_key, base, |x, y, c| {
                    let gx = physical.x + x + dx;
                    let gy = physical.y + y + dy;
                    if gx < 0 || gy < 0 || gx >= pw || gy >= ph {
                        return;
                    }
                    let idx = (gy * pw + gx) as usize;
                    let dst = pixels[idx];
                    let a = c.a() as u32;
                    if a == 0 {
                        return;
                    }
                    let mix = |s: u8, d: u8| -> u8 { ((s as u32 * a + d as u32 * (255 - a)) / 255) as u8 };
                    if let Some(px) = PremultipliedColorU8::from_rgba(
                        mix(c.r(), dst.red()),
                        mix(c.g(), dst.green()),
                        mix(c.b(), dst.blue()),
                        255,
                    ) {
                        pixels[idx] = px;
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airsnip_core::draw::text::TextSink;

    fn ink_bbox(p: &tiny_skia::Pixmap) -> Option<(u32, u32, u32, u32)> {
        let (w, h) = (p.width(), p.height());
        let mut min = (u32::MAX, u32::MAX);
        let mut max = (0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                let a = p.pixels()[(y * w + x) as usize].alpha();
                if a > 0 {
                    min.0 = min.0.min(x);
                    min.1 = min.1.min(y);
                    max.0 = max.0.max(x);
                    max.1 = max.1.max(y);
                }
            }
        }
        (min.0 != u32::MAX).then_some((min.0, min.1, max.0, max.1))
    }

    /// 度量必须等于墨迹尺寸，且 draw(pos) 的字形左上角正好落在 pos 上 ——
    /// 序号徽标、尺寸提示的居中全靠这条性质（用行高居中会把数字推下去）。
    #[test]
    fn measure_matches_drawn_ink() {
        let spec = TextSpec::new(24.0);
        let (mw, mh) = with_engine(|sink| sink.measure("1", &spec));
        assert!(mw > 4.0 && mh > 8.0, "度量不该退化：{mw}×{mh}");

        let mut p = tiny_skia::Pixmap::new(120, 120).unwrap();
        p.fill(tiny_skia::Color::TRANSPARENT);
        with_engine(|sink| {
            sink.draw(
                &mut p,
                airsnip_core::editor::Point::new(40.0, 40.0),
                "1",
                &spec,
                [255, 0, 0, 255],
                tiny_skia::Transform::identity(),
            )
        });
        let (x0, y0, x1, y1) = ink_bbox(&p).expect("应画出字形");
        assert!((x0 as f32 - 40.0).abs() <= 1.0, "墨迹左边应贴住 pos.x：{x0}");
        assert!((y0 as f32 - 40.0).abs() <= 1.0, "墨迹上边应贴住 pos.y：{y0}");
        assert!(
            ((x1 - x0) as f32 - mw).abs() <= 2.0,
            "墨迹宽 {} 应约等于度量 {mw}",
            x1 - x0 + 1
        );
        assert!(
            ((y1 - y0) as f32 - mh).abs() <= 2.0,
            "墨迹高 {} 应约等于度量 {mh}",
            y1 - y0 + 1
        );
    }

    /// 按「中心 - 尺寸/2」放置时，字形四边留白应基本对称（序号徽标靠这条）。
    #[test]
    fn glyph_is_visually_centered() {
        for label in ["1", "8", "10", "888"] {
            let spec = TextSpec::new(24.0);
            let (w, h) = with_engine(|sink| sink.measure(label, &spec));
            let mut p = tiny_skia::Pixmap::new(200, 200).unwrap();
            p.fill(tiny_skia::Color::TRANSPARENT);
            with_engine(|sink| {
                sink.draw(
                    &mut p,
                    airsnip_core::editor::Point::new(100.0 - w / 2.0, 100.0 - h / 2.0),
                    label,
                    &spec,
                    [255, 255, 255, 255],
                    tiny_skia::Transform::identity(),
                )
            });
            let (x0, y0, x1, y1) = ink_bbox(&p).expect("应画出字形");
            let (left, right) = (x0 as i64, 199 - x1 as i64);
            let (top, bottom) = (y0 as i64, 199 - y1 as i64);
            assert!((left - right).abs() <= 2, "{label} 水平不对称：左 {left} 右 {right}");
            assert!((top - bottom).abs() <= 2, "{label} 垂直不对称：上 {top} 下 {bottom}");
        }
    }
}
