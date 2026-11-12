// SPDX-License-Identifier: GPL-3.0-only
//! 截图工具栏（EDT-1…EDT-15、EDT-7、OUT-1/OUT-2）：选区下方单行自绘工具栏。
//!
//! 与 Windows 版同样的分组：**工具 10 个 ｜ 5 个色块 ｜ 填充·撤销·重做 ｜ 复制·保存**。
//! 图标复用 `airsnip_core::draw::icon`（Material Symbols，同一批 SVG）。
//!
//! 布局每帧按选区现算（纯函数，无隐藏状态），命中测试与绘制共用同一份 `Toolbar`，
//! 所以「画在哪」与「点在哪」不可能走形。

use tiny_skia::{Paint, Pixmap, Rect, Transform};

use airsnip_core::draw::icon::{self, Icon};
use airsnip_core::editor::{DEFAULT_COLORS, Tool};

use super::state::Sel;

/// 按钮边长（物理像素，1080p 基准）。
const BTN: i32 = 34;
/// 图标活动区边长。
const ICON: f32 = 19.0;
/// 色块边长。
const SWATCH: i32 = 22;
/// 工具栏内边距。
const PAD: i32 = 5;
/// 元素间距。
const GAP: i32 = 2;
/// 分隔线两侧留白。
const SEP_PAD: i32 = 6;
/// 工具栏高度。
pub const HEIGHT: i32 = BTN + PAD * 2;
/// 与选区的间隔。
const OFFSET: i32 = 10;

const BG: [u8; 4] = [28, 28, 30, 242];
const ICON_COLOR: [u8; 4] = [235, 235, 235, 255];
const SEP_COLOR: [u8; 4] = [255, 255, 255, 44];
const HOVER: [u8; 4] = [255, 255, 255, 26];
const ACCENT: [u8; 4] = [26, 115, 232, 255];
const ACCENT_SOFT: [u8; 4] = [26, 115, 232, 120];

/// 工具栏里的一个元素。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Tool(Tool),
    Color(usize),
    Fill,
    Undo,
    Redo,
    Pin,
    Copy,
    Save,
    /// 分隔线（不可点）。
    Sep,
}

/// 一个元素的矩形（画布坐标）。
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub item: Item,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 工具栏布局结果。
pub struct Toolbar {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub slots: Vec<Slot>,
}

/// 工具 → 图标。
fn tool_icon(t: Tool) -> Icon {
    match t {
        Tool::Rect => Icon::Rect,
        Tool::Arrow => Icon::Arrow,
        Tool::Text => Icon::Text,
        Tool::Highlight => Icon::Highlight,
        Tool::Blur => Icon::Blur,
        Tool::Glow => Icon::Glow,
        Tool::Number => Icon::Number,
        Tool::Line => Icon::Line,
        Tool::Ellipse => Icon::Ellipse,
        Tool::Freehand => Icon::Freehand,
    }
}

/// 元素顺序（与 Windows 版一致）。
fn items() -> Vec<Item> {
    let mut v: Vec<Item> = Tool::ALL.iter().map(|t| Item::Tool(*t)).collect();
    v.push(Item::Sep);
    v.extend((0..DEFAULT_COLORS.len()).map(Item::Color));
    v.push(Item::Sep);
    v.extend([Item::Fill, Item::Undo, Item::Redo]);
    v.push(Item::Sep);
    v.extend([Item::Pin, Item::Copy, Item::Save]);
    v
}

fn item_width(item: Item) -> i32 {
    match item {
        Item::Color(_) => SWATCH,
        Item::Sep => 1 + SEP_PAD * 2,
        _ => BTN,
    }
}

/// 按选区计算工具栏位置（下方优先，塞不下翻到上方，再钳进画布）。
pub fn layout(canvas_w: i32, canvas_h: i32, sel: Sel) -> Toolbar {
    let items = items();
    let inner: i32 = items.iter().map(|i| item_width(*i)).sum::<i32>() + GAP * (items.len() as i32 - 1).max(0);
    let w = inner + PAD * 2;
    let h = HEIGHT;

    let mut y = sel.y + sel.h + OFFSET;
    if y + h > canvas_h {
        y = sel.y - OFFSET - h;
    }
    y = y.clamp(0, (canvas_h - h).max(0));
    let x = (sel.x + sel.w / 2 - w / 2).clamp(0, (canvas_w - w).max(0));

    let mut slots = Vec::with_capacity(items.len());
    let mut cx = x + PAD;
    for item in items {
        let iw = item_width(item);
        slots.push(Slot {
            item,
            x: cx,
            y: y + PAD,
            w: if item == Item::Sep { 1 } else { iw },
            h: if item == Item::Sep { h - PAD * 2 } else { BTN },
        });
        cx += iw + GAP;
    }
    Toolbar { x, y, w, h, slots }
}

impl Toolbar {
    /// 命中测试（画布坐标）。
    pub fn hit(&self, x: i32, y: i32) -> Option<Item> {
        if x < self.x || y < self.y || x >= self.x + self.w || y >= self.y + self.h {
            return None;
        }
        for s in &self.slots {
            if s.item == Item::Sep {
                continue;
            }
            if x >= s.x && y >= s.y && x < s.x + s.w && y < s.y + s.h {
                return Some(s.item);
            }
        }
        // 点在工具栏空白处：吞掉（避免误开始新的框选）
        Some(Item::Sep)
    }
}

/// 绘制工具栏。`active_tool` / `color_index` / `filled` 决定高亮态，
/// `hover` 为指针当前位置（用于按钮悬停底色）。
pub fn paint(
    p: &mut Pixmap,
    tb: &Toolbar,
    active_tool: Tool,
    color_index: usize,
    filled: bool,
    hover: Option<(i32, i32)>,
) {
    round_rect(p, tb.x, tb.y, tb.w, tb.h, 8.0, BG);

    for s in &tb.slots {
        if s.item == Item::Sep {
            let x = s.x as f32 + 0.5;
            fill(
                p,
                Rect::from_xywh(x, s.y as f32 + 4.0, 1.0, (s.h - 8) as f32),
                SEP_COLOR,
            );
            continue;
        }
        // 悬停 / 选中态底色
        let hovered = hover
            .map(|(hx, hy)| hx >= s.x && hy >= s.y && hx < s.x + s.w && hy < s.y + s.h)
            .unwrap_or(false);
        let active = match s.item {
            Item::Tool(t) => t == active_tool,
            Item::Color(i) => i == color_index,
            Item::Fill => filled,
            _ => false,
        };
        if active {
            round_rect(p, s.x, s.y, s.w, s.h, 7.0, ACCENT_SOFT);
        } else if hovered {
            round_rect(p, s.x, s.y, s.w, s.h, 7.0, HOVER);
        }

        let cx = s.x as f32 + s.w as f32 / 2.0;
        let cy = s.y as f32 + s.h as f32 / 2.0;
        match s.item {
            Item::Tool(t) => icon::draw(p, tool_icon(t), cx, cy, ICON, ICON_COLOR),
            Item::Color(i) => {
                let c = DEFAULT_COLORS[i];
                round_rect(p, s.x, s.y, s.w, s.h, 5.0, [c[0], c[1], c[2], 255]);
                if i == color_index {
                    // 选中色块描白圈
                    if let Some(path) = airsnip_core::draw::objects::rounded_rect_path(
                        s.x as f32 + 1.0,
                        s.y as f32 + 1.0,
                        (s.x + s.w - 1) as f32,
                        (s.y + s.h - 1) as f32,
                        4.0,
                    ) {
                        let mut paint = Paint::default();
                        paint.set_color_rgba8(255, 255, 255, 235);
                        p.stroke_path(
                            &path,
                            &paint,
                            &tiny_skia::Stroke {
                                width: 2.0,
                                ..Default::default()
                            },
                            Transform::identity(),
                            None,
                        );
                    }
                }
            }
            Item::Fill => icon::draw(p, Icon::Fill, cx, cy, ICON, icon_color(filled)),
            Item::Undo => icon::draw(p, Icon::Undo, cx, cy, ICON, ICON_COLOR),
            Item::Redo => icon::draw(p, Icon::Redo, cx, cy, ICON, ICON_COLOR),
            Item::Pin => icon::draw(p, Icon::Pin, cx, cy, ICON, ICON_COLOR),
            Item::Copy => icon::draw(p, Icon::Copy, cx, cy, ICON, ICON_COLOR),
            Item::Save => icon::draw(p, Icon::Save, cx, cy, ICON, ICON_COLOR),
            Item::Sep => {}
        }
    }
}

fn icon_color(active: bool) -> [u8; 4] {
    if active { ACCENT } else { ICON_COLOR }
}

/// 圆角矩形填充。
fn round_rect(p: &mut Pixmap, x: i32, y: i32, w: i32, h: i32, r: f32, c: [u8; 4]) {
    let Some(path) =
        airsnip_core::draw::objects::rounded_rect_path(x as f32, y as f32, (x + w) as f32, (y + h) as f32, r)
    else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

fn fill(p: &mut Pixmap, rect: Option<Rect>, c: [u8; 4]) {
    let Some(rect) = rect else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.fill_rect(rect, &paint, Transform::identity(), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel() -> Sel {
        Sel {
            x: 500,
            y: 300,
            w: 400,
            h: 300,
        }
    }

    #[test]
    fn toolbar_sits_below_selection() {
        let tb = layout(1920, 1080, sel());
        assert!(tb.y >= 300 + 300, "应在选区下方：y={}", tb.y);
        assert_eq!(tb.h, HEIGHT);
        // 水平居中于选区
        let center = tb.x + tb.w / 2;
        assert!((center - 700).abs() <= 2, "center={center}");
    }

    #[test]
    fn flips_above_when_no_room_below() {
        let s = Sel {
            x: 100,
            y: 700,
            w: 200,
            h: 350,
        };
        let tb = layout(1920, 1080, s);
        assert!(tb.y + tb.h <= s.y, "塞不下时应翻到上方");
    }

    #[test]
    fn clamps_into_canvas() {
        let s = Sel {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let tb = layout(1920, 1080, s);
        assert!(tb.x >= 0 && tb.x + tb.w <= 1920);
        // 窄屏时工具栏比画面还宽：仍从 0 开始画（右侧裁掉，ADR 0033 同样的已知边界）
        let narrow = layout(400, 1080, s);
        assert_eq!(narrow.x, 0);
    }

    #[test]
    fn every_item_hits_itself() {
        let tb = layout(1920, 1080, sel());
        for s in &tb.slots {
            if s.item == Item::Sep {
                continue;
            }
            let cx = s.x + s.w / 2;
            let cy = s.y + s.h / 2;
            assert_eq!(tb.hit(cx, cy), Some(s.item), "命中失败：{:?}", s.item);
        }
    }

    #[test]
    fn outside_is_ignored() {
        let tb = layout(1920, 1080, sel());
        assert_eq!(tb.hit(tb.x - 5, tb.y), None);
        assert_eq!(tb.hit(tb.x + 2, tb.y + 2), Some(Item::Sep), "空白处应吞掉点击");
    }

    #[test]
    fn groups_are_separated() {
        let tb = layout(1920, 1080, sel());
        let seps = tb.slots.iter().filter(|s| s.item == Item::Sep).count();
        assert_eq!(seps, 3, "工具/颜色/编辑/文件 三处分隔");
        let tools = tb.slots.iter().filter(|s| matches!(s.item, Item::Tool(_))).count();
        assert_eq!(tools, 10);
        let colors = tb.slots.iter().filter(|s| matches!(s.item, Item::Color(_))).count();
        assert_eq!(colors, 5);
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use crate::overlay::state::{Overlay, Sel};

    /// 端到端把工具栏画出来：确认图标真的落到像素上（并可选落盘供肉眼检查）。
    #[test]
    fn toolbar_renders_into_display() {
        let canvas = crate::canvas::Canvas::new(900, 320);
        let mut ov = Overlay::new(&canvas).unwrap();
        ov.sel = Some(Sel {
            x: 120,
            y: 60,
            w: 620,
            h: 150,
        });
        crate::overlay::paint::rebuild(&mut ov);

        let tb = layout(900, 320, ov.current_sel().unwrap());
        // 工具栏区域应有明显亮于暗化底图的像素（图标 + 色块）
        let mut bright = 0;
        for y in tb.y..(tb.y + tb.h) {
            for x in tb.x..(tb.x + tb.w) {
                let i = ((y as usize) * 900 + x as usize) * 4;
                if ov.display.data()[i] > 200 {
                    bright += 1;
                }
            }
        }
        assert!(bright > 200, "工具栏应有可见内容，实际亮像素={bright}");

        if std::env::var_os("AIRSNIP_DEBUG").is_some() {
            let p = crate::draw::pixmap_to_canvas(&ov.display);
            crate::codec::write_png(&p, std::path::Path::new("/tmp/airsnip-toolbar.png")).unwrap();
        }
    }
}
