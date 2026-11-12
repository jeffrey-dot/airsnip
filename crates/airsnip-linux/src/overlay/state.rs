// SPDX-License-Identifier: GPL-3.0-only
//! 覆盖层会话状态（CAP-3/CAP-4、EDT-1…EDT-15）：冻结画面、暗化副本、显示缓冲、
//! 标注文档、当前工具与样式、选区。
//!
//! 三层位图（与 Windows 版同构）：
//! - `bright`：抓屏原图（选区恢复亮度、导出烘焙都用它）
//! - `dim`：暗化副本（会话开始时算一次，之后只做拷贝）
//! - `display`：真正上屏的一帧 = `dim` + 选区亮部 + 标注对象 + 选区装饰
//!
//! 坐标一律物理像素（画布坐标）；每个输出的 surface 逻辑坐标换算在 `surface.rs`。

use tiny_skia::Pixmap;

use airsnip_core::editor::{
    DEFAULT_BLUR_RADIUS, DEFAULT_COLORS, DEFAULT_LINE_WIDTH, DEFAULT_NUMBER_FONT_SIZE, Document, Object, Style, Tool,
};

use crate::canvas::Canvas;

/// 暗化系数（CAP-3）：与 Windows 版 `overlay::surface::DIM_K` 同一个数值。
pub const DIM_K: u32 = 255 - 120;

/// 选区（物理像素，画布坐标）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sel {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Sel {
    pub fn from_drag(a: (i32, i32), b: (i32, i32)) -> Self {
        let x = a.0.min(b.0);
        let y = a.1.min(b.1);
        Self {
            x,
            y,
            w: (a.0 - b.0).abs(),
            h: (a.1 - b.1).abs(),
        }
    }

    /// 松手时小于 3px 视为误触。
    pub fn usable(&self) -> bool {
        self.w >= 3 && self.h >= 3
    }

    pub fn clamped(&self, cw: i32, ch: i32) -> Self {
        let x0 = self.x.clamp(0, cw.max(1));
        let y0 = self.y.clamp(0, ch.max(1));
        let x1 = (self.x + self.w).clamp(0, cw.max(1));
        let y1 = (self.y + self.h).clamp(0, ch.max(1));
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0),
            h: (y1 - y0).max(0),
        }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// 整数像素区域的钳制（标注对象限制在选区内，与 Windows 版一致）。
    pub fn clamp_point(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.x, (self.x + self.w - 1).max(self.x)),
            y.clamp(self.y, (self.y + self.h - 1).max(self.y)),
        )
    }
}

/// 会话结束后的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Save,
    /// 贴图：把导出图钉在屏幕上（子进程常驻）。
    Pin,
    Cancel,
}

/// 覆盖层会话状态。
pub struct Overlay {
    pub bright: Pixmap,
    pub dim: Pixmap,
    pub display: Pixmap,
    pub doc: Document,
    pub tool: Tool,
    pub style: Style,
    pub filled: bool,
    /// 当前色块下标（工具栏高亮用）。
    pub color_index: usize,
    /// 按下点落在工具栏上：松手时不要结束框选/结束绘制。
    pub toolbar_hold: bool,
    pub blur_radius: f32,
    pub number_font_size: f32,
    /// 正在拖拽的草稿对象。
    pub draft: Option<Object>,
    pub sel: Option<Sel>,
    pub drag_from: Option<(i32, i32)>,
    /// 当前手势是否在画标注（false = 在框选）。
    pub drawing: bool,
    pub pointer: Option<(i32, i32)>,
    pub active: Option<usize>,
    pub layers: Vec<crate::overlay::surface::Layer>,
    pub serial: u32,
    pub ctrl: bool,
    pub done: bool,
    pub action: Option<Action>,
    /// 正在输入文本（EDT-4）；`None` 表示不在文本编辑态。
    /// 选中的对象下标（EDT-7，S4c）。
    pub selected: Option<usize>,
    /// 正在进行的对象拖动 / 缩放。
    pub edit: Option<super::objedit::Drag>,
    /// 按下时压在某个对象上：(对象下标, 按下时的对象数)。
    /// 松手时若对象数没变 → 这次是"单击选中"；变了 → 是"在对象上画了新标注"。
    pub pending_select: Option<(usize, usize)>,
    pub text_edit: Option<super::text_input::TextEdit>,
    /// 输入法是否已接管按键（收到过 commit/preedit 事件）。
    pub ime_active: bool,
    /// Shift 是否按住（无输入法时的字符大小写）。
    pub shift: bool,
}

impl Overlay {
    /// 由冻结画面建会话；内存分配失败时返回 None。
    pub fn new(canvas: &Canvas) -> Option<Self> {
        let bright = crate::draw::canvas_to_pixmap(canvas)?;
        let mut dim = bright.clone();
        dim_pixmap(&mut dim);
        let display = dim.clone();
        Some(Self {
            bright,
            dim,
            display,
            doc: Document::new(),
            tool: Tool::Rect,
            // 默认选红（与 Windows 版一致）：荧光笔是正片叠底，白色 = 不变，
            // 白底默认色会让「高亮」看起来完全没反应。
            style: Style::new(DEFAULT_COLORS[1], DEFAULT_LINE_WIDTH),
            filled: false,
            color_index: 1,
            toolbar_hold: false,
            blur_radius: DEFAULT_BLUR_RADIUS,
            number_font_size: DEFAULT_NUMBER_FONT_SIZE,
            draft: None,
            sel: None,
            drag_from: None,
            drawing: false,
            pointer: None,
            active: None,
            layers: Vec::new(),
            serial: 0,
            ctrl: false,
            done: false,
            action: None,
            selected: None,
            edit: None,
            pending_select: None,
            text_edit: None,
            ime_active: false,
            shift: false,
        })
    }

    pub fn finish(&mut self, action: Action) {
        if self.done {
            return;
        }
        self.done = true;
        self.action = Some(action);
    }

    pub fn current_sel(&self) -> Option<Sel> {
        self.sel.filter(Sel::usable)
    }
}

/// 暗化（RGB 乘 `DIM_K/256`）。
fn dim_pixmap(p: &mut Pixmap) {
    for px in p.data_mut().chunks_exact_mut(4) {
        px[0] = ((px[0] as u32 * DIM_K) >> 8) as u8;
        px[1] = ((px[1] as u32 * DIM_K) >> 8) as u8;
        px[2] = ((px[2] as u32 * DIM_K) >> 8) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_normalizes_direction() {
        let s = Sel::from_drag((100, 80), (20, 10));
        assert_eq!(
            s,
            Sel {
                x: 20,
                y: 10,
                w: 80,
                h: 70
            }
        );
    }

    #[test]
    fn tiny_drag_is_rejected() {
        assert!(!Sel::from_drag((10, 10), (12, 40)).usable());
        assert!(Sel::from_drag((10, 10), (40, 40)).usable());
    }

    #[test]
    fn clamp_to_canvas() {
        let s = Sel::from_drag((-10, -10), (30, 30)).clamped(100, 100);
        assert_eq!(
            s,
            Sel {
                x: 0,
                y: 0,
                w: 30,
                h: 30
            }
        );
        let s = Sel::from_drag((90, 90), (200, 200)).clamped(100, 100);
        assert_eq!(
            s,
            Sel {
                x: 90,
                y: 90,
                w: 10,
                h: 10
            }
        );
    }

    #[test]
    fn intersect_and_contains() {
        let s = Sel {
            x: 10,
            y: 10,
            w: 100,
            h: 50,
        };
        assert!(s.contains(10, 10));
        assert!(!s.contains(110, 60));
        assert_eq!(s.clamp_point(0, 0), (10, 10));
        assert_eq!(s.clamp_point(999, 999), (109, 59));
    }

    #[test]
    fn dim_matches_windows_factor() {
        let mut c = Canvas::new(1, 1);
        c.px.copy_from_slice(&[255, 255, 255, 255]);
        let p = crate::draw::canvas_to_pixmap(&c).unwrap();
        let mut p2 = p.clone();
        dim_pixmap(&mut p2);
        assert_eq!(&p2.data()[..3], &[134, 134, 134]);
    }
}
