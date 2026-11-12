// SPDX-License-Identifier: GPL-3.0-only
//! 文本渲染的平台接口（ADR 0034）：core 负责「对象 → 像素」，但**字形栅格化**由各前端提供。
//!
//! Windows 用 DirectWrite（`crate::render::text`），Linux 用 cosmic-text（`crate::draw::text`）。
//! core 只认这个 trait，不认任何平台 API——所以 `draw_objects` 之类的绘制函数多了一个
//! `&dyn TextSink` 参数，两端各自传实现。

use std::sync::Arc;

use tiny_skia::{Pixmap, Transform};

use crate::editor::Point;

/// 文本样式（与 Windows 版 `render::text::FontSpec` 一一对应）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub family: Arc<str>,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
}

impl TextSpec {
    pub fn new(size: f32) -> Self {
        Self {
            family: Arc::from(crate::editor::DEFAULT_FONT_FAMILY),
            size,
            bold: false,
            italic: false,
        }
    }
}

/// 文本引擎：度量与栅格化。
pub trait TextSink {
    /// 文本尺寸（宽, 高），不含内边距。
    fn measure(&self, text: &str, spec: &TextSpec) -> (f32, f32);

    /// 文字块与**视觉中心**的垂直偏差补偿（物理像素）。
    ///
    /// 各平台文本引擎对"行盒"的定义不同：DirectWrite 的布局盒含额外 leading，
    /// 用行盒居中时数字会偏下，需要一点补偿；cosmic-text 侧我们直接返回**墨迹**尺寸，
    /// 因此补偿为 0。默认 0，只有需要的实现覆盖它。
    fn vertical_bias(&self, _font_size: f32) -> f32 {
        0.0
    }

    /// 以 `pos` 为左上角绘制到 `pixmap`（`tr` 为坐标系变换）。
    fn draw(&self, pixmap: &mut Pixmap, pos: Point, text: &str, spec: &TextSpec, color: [u8; 4], tr: Transform);
}

/// 不画字的实现：给没有文本能力的场景（单测、纯图形导出）用。
pub struct NoText;

impl TextSink for NoText {
    fn measure(&self, text: &str, spec: &TextSpec) -> (f32, f32) {
        // 退化估算：等宽近似，保证布局不会塌成 0（仅供测试与兜底）。
        let w = text.chars().count() as f32 * spec.size * 0.6;
        (w, spec.size * 1.2)
    }

    fn draw(&self, _pixmap: &mut Pixmap, _pos: Point, _text: &str, _spec: &TextSpec, _color: [u8; 4], _tr: Transform) {}
}
