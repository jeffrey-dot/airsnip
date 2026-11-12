// SPDX-License-Identifier: GPL-3.0-only
//! 矢量渲染（PRD 6.3.2/6.3.3）：**Windows 前端的薄壳**。
//!
//! 对象 → 像素的绘制逻辑（矩形/箭头/文本/模糊/高亮/序号/直线/椭圆/手绘、导出烘焙、
//! 脏区求交）已搬到 `airsnip_core::draw`（ADR 0034），两个前端共用一份实现。
//! 这里只做两件事：
//!
//! 1. 把 DirectWrite 的文本引擎接上 core 的 `TextSink` 接口（Linux 侧接 cosmic-text）；
//! 2. 给出**不改变原调用点**的包装函数——调用方仍写 `render::draw_objects(…)`。
//!
//! `text` / `fonts` / `text_bitmap` 仍留在 Windows 侧：它们直接调 DirectWrite。

pub use airsnip_core::draw::{blur, icon, mask, region, stroke};

pub mod fonts;
pub mod text;
mod text_bitmap;

use tiny_skia::{Pixmap, Transform};

use airsnip_core::draw::objects;
use airsnip_core::draw::text::{TextSink, TextSpec};
use airsnip_core::editor::{Bounds, Object, Point};

/// 像素区域（x0, y0, x1, y1 半开区间，物理像素）。
pub type Clip = (i32, i32, i32, i32);

/// DirectWrite 支持的 `TextSink`。
struct DWriteSink;

impl TextSink for DWriteSink {
    fn measure(&self, s: &str, spec: &TextSpec) -> (f32, f32) {
        text::measure(s, &font_spec(spec))
    }

    /// DirectWrite 的布局盒含额外 leading，序号徽标居中时历史上就有这个补偿。
    fn vertical_bias(&self, font_size: f32) -> f32 {
        font_size * 0.08
    }

    fn draw(&self, pixmap: &mut Pixmap, pos: Point, s: &str, spec: &TextSpec, color: [u8; 4], tr: Transform) {
        text::draw(pixmap, pos, s, &font_spec(spec), color, tr);
    }
}

fn font_spec(spec: &TextSpec) -> text::FontSpec {
    text::FontSpec::from_parts(&spec.family, spec.size, spec.bold, spec.italic)
}

/// 绘制对象列表；`draft` 是正在拖拽的临时对象，绘制在最上层。
pub fn draw_objects(pixmap: &mut Pixmap, objects: &[Object], draft: Option<&Object>, skip: Option<usize>, clip: Clip) {
    airsnip_core::draw::objects::draw_objects(pixmap, objects, draft, skip, clip, &DWriteSink);
}

/// 绘制单个对象。
pub fn draw_object(pixmap: &mut Pixmap, obj: &Object) {
    airsnip_core::draw::objects::draw_object(pixmap, obj, &DWriteSink);
}

/// 绘制单个对象，并施加坐标系变换（导出烘焙时用于平移到裁剪图坐标）。
pub fn draw_object_with(pixmap: &mut Pixmap, obj: &Object, transform: Transform) {
    airsnip_core::draw::objects::draw_object_with_text(pixmap, obj, transform, &DWriteSink);
}

/// 对象包围盒是否与像素区域相交。
pub fn intersects(b: &Bounds, clip: Clip) -> bool {
    objects::intersects(b, clip)
}

/// 对象包围盒向外取整为像素区域，供脏区计算。
pub fn bounds_to_clip(b: &Bounds) -> Clip {
    objects::bounds_to_clip(b)
}

/// EDT-12 徽标圆半径（按编号位数测量）。
pub fn number_radius(font_size: f32, n: u32) -> f32 {
    objects::number_radius(font_size, n, &DWriteSink)
}

/// 导出烘焙：把对象栅格化进裁剪结果图，并处理聚光（Glow）。
pub fn bake_objects(pixmap: &mut Pixmap, objects: &[Object], origin: (i32, i32)) {
    airsnip_core::draw::bake::bake_objects(pixmap, objects, origin, &DWriteSink);
}
