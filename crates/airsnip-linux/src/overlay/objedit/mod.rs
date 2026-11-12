// SPDX-License-Identifier: GPL-3.0-only
//! 对象选中与编辑（EDT-7，S4c）：包围框、八向手柄、拖动移动、缩放、删除。
//!
//! 与 Windows 版同一套交互约定（ADR 0023）：
//! - **点空白** → 取消选中，照常开始新的框选/绘制；
//! - **点中对象** → 选中（不移动，避免和"在对象上继续画"打架）；
//! - **`Ctrl` + 拖动对象** → 移动；
//! - **拖动选中对象的八向手柄** → 缩放（箭头 / 直线只有两端点手柄）；
//! - `Del` / `Backspace` → 删除，`Ctrl+Z` 一步恢复；方向键微调（`Shift` ×5）。
//!
//! 几何运算全部复用 core：`Kind::hit` 命中、`Kind::translate` 平移、
//! `Kind::remap` 按包围框重映射（文本字号、手绘点列都会跟着缩放）。

use airsnip_core::editor::{Bounds, Kind, Object, Point};

pub mod paint;

use super::state::{Overlay, Sel};

/// 手柄边长（物理像素）。
pub const HANDLE: f32 = 7.0;
/// 手柄命中半径。
pub const TOL: f32 = 7.0;
/// 对象最小尺寸（防止缩成一条线后再也点不中）。
const MIN_SIZE: f32 = 6.0;
/// 圆角手柄的基准内缩。
const RADIUS_GRIP: f32 = 14.0;
/// 半径 → 手柄内缩的系数（圆弧到角的距离比例）。
const ARC_TO_CORNER: f32 = 0.293;

/// 编辑手柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    /// 箭头 / 直线的起点。
    Start,
    /// 箭头 / 直线的终点。
    End,
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    /// 箭头曲率控制点（1/3、2/3 处）。
    Ctrl1,
    Ctrl2,
    /// 圆角手柄（右侧内侧小圆）。
    Radius,
}

/// 正在进行的拖动。
#[derive(Clone, Debug)]
pub enum Drag {
    Move {
        /// 拖动起点（画布坐标）。
        from: Point,
        /// 按下时对象的包围框。
        bounds: Bounds,
        /// 按下时的对象几何。**每帧都从它重算**——若在"已移动过的对象"上
        /// 再做一次 remap，位移会逐帧累积（拖 10 帧就跑出去 10 倍）。
        kind: Kind,
    },
    Resize {
        handle: Handle,
        bounds: Bounds,
        kind: Kind,
    },
    /// 拖箭头曲率控制点。
    Curve {
        which: Handle,
        kind: Kind,
    },
    /// 拖圆角手柄。
    Radius {
        kind: Kind,
        /// 起始圆角半径（每帧按总位移重算，不累积）。
        corner: f32,
        from: Point,
    },
}

/// 选中对象的包围框（整数像素，供手柄定位）。
pub fn frame(obj: &Object) -> Sel {
    let b = obj.bounds();
    Sel {
        x: b.x0.floor() as i32,
        y: b.y0.floor() as i32,
        w: (b.x1.ceil() - b.x0.floor()).max(1.0) as i32,
        h: (b.y1.ceil() - b.y0.floor()).max(1.0) as i32,
    }
}

fn near(a: Point, b: Point, tol: f32) -> bool {
    (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol
}

/// 命中的手柄（按对象类型区分：箭头 / 直线只有两端点）。
pub fn handle_at(obj: &Object, p: Point) -> Option<Handle> {
    // 圆角手柄压在最上面，优先命中
    if let Some((x, y)) = radius_handle_point(obj)
        && near(Point::new(x, y), p, TOL)
    {
        return Some(Handle::Radius);
    }
    // 箭头的曲率控制点（1/3、2/3）
    if let Some([c1, c2]) = obj.kind.control_points() {
        if near(c1, p, TOL) {
            return Some(Handle::Ctrl1);
        }
        if near(c2, p, TOL) {
            return Some(Handle::Ctrl2);
        }
    }
    match &obj.kind {
        Kind::Arrow { from, to, .. } | Kind::Line { from, to } => {
            if near(*from, p, TOL) {
                return Some(Handle::Start);
            }
            if near(*to, p, TOL) {
                return Some(Handle::End);
            }
            return None;
        }
        _ => {}
    }
    let b = obj.bounds();
    let (cx, cy) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
    let pts = [
        (Handle::NW, b.x0, b.y0),
        (Handle::N, cx, b.y0),
        (Handle::NE, b.x1, b.y0),
        (Handle::E, b.x1, cy),
        (Handle::SE, b.x1, b.y1),
        (Handle::S, cx, b.y1),
        (Handle::SW, b.x0, b.y1),
        (Handle::W, b.x0, cy),
    ];
    pts.iter()
        .find(|(_, x, y)| near(Point::new(*x, *y), p, TOL))
        .map(|(h, _, _)| *h)
}

/// 严格命中（点在线身/描边上）：判定「这一下是画，还是点中对象」。
#[allow(dead_code)] // 供后续"画在对象上"的判定复用
pub fn hit_object(ov: &Overlay, p: Point) -> Option<usize> {
    // 容差含线宽的一半，粗线也好点中
    ov.doc
        .objects()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, o)| o.kind.hit(p, o.style.width.max(4.0) * 0.5 + 2.0))
        .map(|(i, _)| i)
}

/// 宽松命中（包围框内即算）：用于**选中/移动**。
///
/// 严格命中对未填充矩形只有描边算中，用户点内部会变成"又画一个矩形"——
/// 选中/移动用包围框判定更符合直觉；"画还是选"仍由严格命中 + 有无位移决定
/// （Windows 版 ADR 0023 的「无位移松手 = 选中」）。
pub fn hit_object_loose(ov: &Overlay, p: Point) -> Option<usize> {
    ov.doc
        .objects()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, o)| {
            let b = o.bounds();
            p.x >= b.x0 && p.x <= b.x1 && p.y >= b.y0 && p.y <= b.y1
        })
        .map(|(i, _)| i)
}

/// 按下：选中并开始拖动（`ctrl` 才允许移动本体，与 Windows 版一致）。
pub fn press(ov: &mut Overlay, p: (i32, i32), ctrl: bool) -> bool {
    let pt = Point::new(p.0 as f32, p.1 as f32);
    // 1) 先试选中对象的手柄
    if let Some(idx) = ov.selected {
        let hit = ov
            .doc
            .objects()
            .get(idx)
            .and_then(|obj| handle_at(obj, pt).map(|h| (h, obj.bounds(), obj.kind.clone())));
        if let Some((handle, bounds, kind)) = hit {
            ov.doc.begin();
            ov.edit = Some(match handle {
                // 箭头曲率控制点
                Handle::Ctrl1 | Handle::Ctrl2 => Drag::Curve { which: handle, kind },
                // 圆角手柄
                Handle::Radius => Drag::Radius {
                    corner: kind.corner().unwrap_or(0.0),
                    kind,
                    from: pt,
                },
                // 端点 / 八向缩放
                _ => Drag::Resize { handle, bounds, kind },
            });
            return true;
        }
    }
    // 2) 再点对象：选中；带 Ctrl 才能拖动本体
    if let Some(idx) = hit_object_loose(ov, pt) {
        let (bounds, kind) = {
            let obj = &ov.doc.objects()[idx];
            (obj.bounds(), obj.kind.clone())
        };
        ov.selected = Some(idx);
        if ctrl {
            ov.doc.begin();
            ov.edit = Some(Drag::Move { from: pt, bounds, kind });
        }
        return true;
    }
    // 3) 空白处：取消选中，交给调用方继续走"框选/绘制"
    ov.selected = None;
    false
}

/// 把样式改动应用到选中对象（颜色 / 填充 / 线宽），归档为一步撤销。
pub fn restyle_selected(ov: &mut Overlay, f: impl Fn(&mut airsnip_core::editor::Object)) {
    let Some(idx) = ov.selected else { return };
    ov.doc.begin();
    if let Some(obj) = ov.doc.objects_mut().get_mut(idx) {
        f(obj);
    }
    ov.doc.commit(true);
}

/// 滚轮调线宽：作用于选中对象，否则改当前画笔。
pub fn adjust_width(ov: &mut Overlay, delta: f32) -> bool {
    if ov.selected.is_some() {
        restyle_selected(ov, |obj| {
            obj.style.width = (obj.style.width + delta).clamp(1.0, 40.0);
        });
        if let Some(idx) = ov.selected
            && let Some(obj) = ov.doc.objects().get(idx)
        {
            crate::wayland::debug(format!("选中对象线宽 → {}", obj.style.width));
        }
        return true;
    }
    ov.style.width = (ov.style.width + delta).clamp(1.0, 40.0);
    crate::wayland::debug(format!("画笔线宽 → {}", ov.style.width));
    false
}

/// 拖动中：移动或缩放。
pub fn drag(ov: &mut Overlay, p: (i32, i32)) {
    let Some(state) = ov.edit.clone() else { return };
    let Some(sel) = ov.sel else { return };
    let Some(idx) = ov.selected else { return };
    let (x, y) = sel.clamp_point(p.0, p.1);
    let pt = Point::new(x as f32, y as f32);
    let Some(obj) = ov.doc.objects_mut().get_mut(idx) else {
        return;
    };
    match state {
        Drag::Move { from, bounds, kind } => {
            let (dx, dy) = (pt.x - from.x, pt.y - from.y);
            // 始终以按下时的几何为基准重算，拖多久都不会累积误差
            let mut moved = kind;
            moved.remap(
                bounds,
                Bounds {
                    x0: bounds.x0 + dx,
                    y0: bounds.y0 + dy,
                    x1: bounds.x1 + dx,
                    y1: bounds.y1 + dy,
                },
            );
            obj.kind = moved;
        }
        Drag::Resize { handle, bounds, kind } => {
            let mut edited = kind;
            match handle {
                // 端点手柄：直接改端点（不缩放整体）
                Handle::Start | Handle::End => set_endpoint(&mut edited, handle, pt),
                Handle::Ctrl1 | Handle::Ctrl2 | Handle::Radius => {}
                _ => edited.remap(bounds, resized(bounds, handle, pt)),
            }
            obj.kind = edited;
        }
        Drag::Curve { which, kind } => {
            let mut edited = kind;
            if let Kind::Arrow { c1, c2, .. } = &mut edited {
                match which {
                    Handle::Ctrl1 => *c1 = pt,
                    Handle::Ctrl2 => *c2 = pt,
                    _ => {}
                }
            }
            obj.kind = edited;
        }
        Drag::Radius { kind, corner, from } => {
            let mut edited = kind;
            let next = corner + radius_delta(pt.x - from.x, pt.y - from.y);
            edited.set_corner(next.max(0.0));
            obj.kind = edited;
        }
    }
}

/// 松手：结束拖动，必要时归档一步撤销。
pub fn release(ov: &mut Overlay) {
    let Some(_) = ov.edit.take() else { return };
    ov.doc.commit(true);
}

/// 取消拖动（Esc 时回滚未完成的几何变更）。
#[allow(dead_code)]
pub fn cancel(ov: &mut Overlay) {
    if ov.edit.take().is_some() {
        ov.doc.commit(false);
        // 撤回这次未完成的几何变更
        ov.doc.undo();
    }
}

/// 选中对象按方向键微调。
pub fn nudge(ov: &mut Overlay, dx: f32, dy: f32) -> bool {
    let Some(idx) = ov.selected else {
        return false;
    };
    ov.doc.begin();
    let moved = match ov.doc.objects_mut().get_mut(idx) {
        Some(obj) => {
            obj.kind.translate(dx, dy);
            true
        }
        None => false,
    };
    ov.doc.commit(moved);
    moved
}

/// 删除选中的对象（一步撤销可恢复）。
pub fn delete(ov: &mut Overlay) -> bool {
    let Some(idx) = ov.selected.take() else {
        return false;
    };
    ov.doc.begin();
    ov.doc.remove(idx);
    ov.doc.commit(true);
    true
}

/// 手柄拖动后的新包围框。
fn resized(from: Bounds, handle: Handle, p: Point) -> Bounds {
    let mut to = from;
    match handle {
        Handle::NW => {
            to.x0 = p.x;
            to.y0 = p.y;
        }
        Handle::N => to.y0 = p.y,
        Handle::NE => {
            to.x1 = p.x;
            to.y0 = p.y;
        }
        Handle::E => to.x1 = p.x,
        Handle::SE => {
            to.x1 = p.x;
            to.y1 = p.y;
        }
        Handle::S => to.y1 = p.y,
        Handle::SW => {
            to.x0 = p.x;
            to.y1 = p.y;
        }
        Handle::W => to.x0 = p.x,
        Handle::Start | Handle::End | Handle::Ctrl1 | Handle::Ctrl2 | Handle::Radius => {}
    }
    // 允许反向拖（拖过对边就翻过来），但不允许缩到看不见
    if to.x1 - to.x0 < MIN_SIZE {
        if to.x0 != from.x0 {
            to.x0 = to.x1 - MIN_SIZE;
        } else {
            to.x1 = to.x0 + MIN_SIZE;
        }
    }
    if to.y1 - to.y0 < MIN_SIZE {
        if to.y0 != from.y0 {
            to.y0 = to.y1 - MIN_SIZE;
        } else {
            to.y1 = to.y0 + MIN_SIZE;
        }
    }
    to
}

/// 端点手柄：改 arrow / line 的端点。
fn set_endpoint(kind: &mut Kind, handle: Handle, p: Point) {
    match (kind, handle) {
        (Kind::Arrow { from, .. }, Handle::Start) | (Kind::Line { from, .. }, Handle::Start) => *from = p,
        (Kind::Arrow { to, c1, c2, .. }, Handle::End) => {
            // 终点移动时把控制点按比例带走，曲线形状不变
            let (dx, dy) = (p.x - to.x, p.y - to.y);
            *c1 = Point::new(c1.x + dx, c1.y + dy);
            *c2 = Point::new(c2.x + dx, c2.y + dy);
            *to = p;
        }
        (Kind::Line { to, .. }, Handle::End) => *to = p,
        _ => {}
    }
}

/// 圆角手柄位置（ADR 0010）：包围框内侧右上角，随半径内缩。
///
/// `inset = 基准内缩 + 0.293 × r`，并夹到短边的 45%——圆弧离角越远，手柄跟着往里走，
/// 这样"拖手柄"的直觉与"改半径"一致（朝中心拖变大、朝角拖变小）。
pub fn radius_handle_point(obj: &Object) -> Option<(f32, f32)> {
    let r = obj.kind.corner()?;
    let f = frame(obj);
    let inset = (RADIUS_GRIP + ARC_TO_CORNER * r)
        .min(f.w as f32 * 0.45)
        .min(f.h as f32 * 0.45);
    Some(((f.x + f.w) as f32 - inset, f.y as f32 + inset))
}

/// 拖拽位移 → 圆角半径变化量。**朝中心拖（左下）增大、朝角拖（右上）减小**。
pub fn radius_delta(dx: f32, dy: f32) -> f32 {
    (dy - dx) * 0.5
}

/// 手柄位置列表（绘制与命中同一份数据）。
pub fn handle_points(obj: &Object) -> Vec<(Handle, Point)> {
    match &obj.kind {
        Kind::Arrow { from, to, .. } | Kind::Line { from, to } => {
            vec![(Handle::Start, *from), (Handle::End, *to)]
        }
        _ => {
            let b = obj.bounds();
            let (cx, cy) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
            vec![
                (Handle::NW, Point::new(b.x0, b.y0)),
                (Handle::N, Point::new(cx, b.y0)),
                (Handle::NE, Point::new(b.x1, b.y0)),
                (Handle::E, Point::new(b.x1, cy)),
                (Handle::SE, Point::new(b.x1, b.y1)),
                (Handle::S, Point::new(cx, b.y1)),
                (Handle::SW, Point::new(b.x0, b.y1)),
                (Handle::W, Point::new(b.x0, cy)),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airsnip_core::editor::{Kind, Object, Style};

    fn rect_obj() -> Object {
        Object {
            kind: Kind::Rect {
                a: Point::new(50.0, 40.0),
                b: Point::new(150.0, 120.0),
                radius: 0.0,
                filled: true,
            },
            style: Style::new([255, 0, 0, 255], 4.0),
        }
    }

    #[test]
    fn handles_land_on_corners_and_midpoints() {
        let obj = rect_obj();
        assert_eq!(handle_at(&obj, Point::new(50.0, 40.0)), Some(Handle::NW));
        assert_eq!(handle_at(&obj, Point::new(150.0, 120.0)), Some(Handle::SE));
        assert_eq!(handle_at(&obj, Point::new(100.0, 40.0)), Some(Handle::N));
        assert_eq!(handle_at(&obj, Point::new(60.0, 60.0)), None);
    }

    #[test]
    fn arrow_only_has_endpoints() {
        let obj = Object {
            kind: Kind::arrow(Point::new(0.0, 0.0), Point::new(100.0, 0.0)),
            style: Style::new([0, 0, 0, 255], 4.0),
        };
        assert_eq!(handle_at(&obj, Point::new(0.0, 0.0)), Some(Handle::Start));
        assert_eq!(handle_at(&obj, Point::new(100.0, 0.0)), Some(Handle::End));
        assert_eq!(handle_at(&obj, Point::new(50.0, 50.0)), None);
    }

    #[test]
    fn resize_grows_towards_handle() {
        let from = rect_obj().bounds();
        let to = resized(from, Handle::SE, Point::new(200.0, 160.0));
        assert_eq!((to.x1, to.y1), (200.0, 160.0));
        assert_eq!((to.x0, to.y0), (from.x0, from.y0));
    }

    #[test]
    fn resize_keeps_minimum_size() {
        let from = rect_obj().bounds();
        let to = resized(from, Handle::SE, Point::new(from.x0, from.y0));
        assert!(to.x1 - to.x0 >= MIN_SIZE - 0.001);
        assert!(to.y1 - to.y0 >= MIN_SIZE - 0.001);
    }

    fn overlay_with_rect() -> Overlay {
        let canvas = crate::canvas::Canvas::new(400, 300);
        let mut ov = Overlay::new(&canvas).unwrap();
        ov.sel = Some(Sel {
            x: 0,
            y: 0,
            w: 400,
            h: 300,
        });
        ov.doc.push(rect_obj());
        ov
    }

    #[test]
    fn press_selects_object_without_drawing() {
        let mut ov = overlay_with_rect();
        assert!(press(&mut ov, (100, 80), false), "点中对象应被吃掉");
        assert_eq!(ov.selected, Some(0));
        assert!(ov.edit.is_none(), "不按 Ctrl 不应开始拖动");
    }

    #[test]
    fn ctrl_drag_moves_object() {
        let mut ov = overlay_with_rect();
        let before = ov.doc.objects()[0].bounds();
        press(&mut ov, (100, 80), true);
        assert!(ov.edit.is_some());
        drag(&mut ov, (140, 110));
        release(&mut ov);
        let after = ov.doc.objects()[0].bounds();
        // 位移 (40, 30)：与按下时的包围框比，看整体平移量
        assert!((after.x0 - before.x0 - 40.0).abs() < 1.0, "dx={}", after.x0 - before.x0);
        assert!((after.y0 - before.y0 - 30.0).abs() < 1.0, "dy={}", after.y0 - before.y0);
    }

    #[test]
    fn repeated_drag_frames_do_not_accumulate() {
        let mut ov = overlay_with_rect();
        let before = ov.doc.objects()[0].bounds();
        press(&mut ov, (100, 80), true);
        // 模拟 10 帧拖动：最后一帧指针在 (200,130)，总位移应是 (100,50)。
        // 早期实现在"已移动过的对象"上再 remap，位移逐帧累积，会跑到 ~(1000,500)。
        for i in 1..=10 {
            drag(&mut ov, (100 + i * 10, 80 + i * 5));
        }
        release(&mut ov);
        let after = ov.doc.objects()[0].bounds();
        assert!(
            (after.x0 - before.x0 - 100.0).abs() < 1.0,
            "dx={}",
            after.x0 - before.x0
        );
        assert!((after.y0 - before.y0 - 50.0).abs() < 1.0, "dy={}", after.y0 - before.y0);
    }

    #[test]
    fn corner_handle_resizes() {
        let mut ov = overlay_with_rect();
        // 先选中，再抓住右下角手柄
        press(&mut ov, (100, 80), false);
        press(&mut ov, (150, 120), false);
        assert!(ov.edit.is_some(), "应按在手柄上进入缩放");
        drag(&mut ov, (250, 200));
        release(&mut ov);
        let b = ov.doc.objects()[0].bounds();
        assert!(b.x1 > 240.0 && b.y1 > 190.0, "应被拉大：{:?}", b);
    }

    #[test]
    fn delete_and_undo() {
        let mut ov = overlay_with_rect();
        press(&mut ov, (100, 80), false);
        assert!(delete(&mut ov));
        assert!(ov.doc.objects().is_empty());
        assert!(ov.selected.is_none());
        assert!(ov.doc.undo());
        assert_eq!(ov.doc.objects().len(), 1);
    }

    #[test]
    fn nudge_moves_selected() {
        let mut ov = overlay_with_rect();
        let before = ov.doc.objects()[0].bounds();
        press(&mut ov, (100, 80), false);
        assert!(nudge(&mut ov, 5.0, 0.0));
        let after = ov.doc.objects()[0].bounds();
        assert!((after.x0 - before.x0 - 5.0).abs() < 0.01, "dx={}", after.x0 - before.x0);
    }

    #[test]
    fn restyle_color_applies_to_selection() {
        let mut ov = overlay_with_rect();
        press(&mut ov, (100, 80), false);
        restyle_selected(&mut ov, |o| o.style.color = [1, 2, 3, 255]);
        assert_eq!(ov.doc.objects()[0].style.color, [1, 2, 3, 255]);
        assert!(ov.doc.undo(), "改色应可撤销");
    }

    #[test]
    fn wheel_changes_width_of_selected() {
        let mut ov = overlay_with_rect();
        press(&mut ov, (100, 80), false);
        adjust_width(&mut ov, 3.0);
        assert_eq!(ov.doc.objects()[0].style.width, 7.0);
    }

    #[test]
    fn curve_control_point_drag_bends_arrow() {
        let mut ov = overlay_with_rect();
        // 换成一个箭头并选中
        ov.doc = airsnip_core::editor::Document::new();
        ov.doc.push(Object {
            kind: Kind::arrow(Point::new(100.0, 100.0), Point::new(300.0, 100.0)),
            style: Style::new([0, 0, 0, 255], 4.0),
        });
        press(&mut ov, (100, 100), false); // 选中（点在起点手柄上也会选中）
        let c1 = match ov.doc.objects()[0].kind.control_points() {
            Some([c1, _]) => c1,
            _ => panic!("箭头应有控制点"),
        };
        // 抓住第一个控制点往上拖
        press(&mut ov, (c1.x.round() as i32, c1.y.round() as i32), false);
        assert!(matches!(ov.edit, Some(Drag::Curve { .. })), "应进入曲线拖动");
        drag(&mut ov, (c1.x.round() as i32, c1.y.round() as i32 - 60));
        release(&mut ov);
        let after = ov.doc.objects()[0].kind.control_points().unwrap()[0];
        assert!((after.y - c1.y).abs() > 30.0, "控制点应被拖走：{:?} → {:?}", c1, after);
    }

    #[test]
    fn radius_handle_drag_rounds_rect() {
        let mut ov = overlay_with_rect();
        press(&mut ov, (100, 80), false); // 选中
        let obj = &ov.doc.objects()[0];
        let (hx, hy) = radius_handle_point(obj).unwrap();
        press(&mut ov, (hx.round() as i32, hy.round() as i32), false);
        assert!(matches!(ov.edit, Some(Drag::Radius { .. })), "应进入圆角拖动");
        // 朝中心（左下）拖 → 半径增大
        drag(&mut ov, (hx.round() as i32 - 40, hy.round() as i32 + 40));
        release(&mut ov);
        let r = ov.doc.objects()[0].kind.corner().unwrap();
        assert!(r > 10.0, "圆角应变大，实际 {r}");
        assert!(ov.doc.undo(), "改圆角应可撤销");
    }

    #[test]
    fn frame_covers_object_bounds() {
        let obj = rect_obj();
        let f = frame(&obj);
        assert!(f.x <= 50 && f.y <= 40);
        assert!(f.x + f.w >= 150 && f.y + f.h >= 120);
    }
}
