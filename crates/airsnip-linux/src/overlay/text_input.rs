// SPDX-License-Identifier: GPL-3.0-only
//! 文本标注（EDT-4）：点击放置 → 输入（输入法或按键）→ 提交为文本对象。
//!
//! 与 Windows 版的差别：Windows 用原生 `EDIT` 子控件（系统负责输入法、光标、选区），
//! Wayland 侧没这个控件，所以这里自己做一层最小编辑态：
//!
//! - **输入法**走 `zwp_text_input_v3`（niri / KDE / sway 均支持）：`enable` 后收
//!   `commit_string` 往编辑态里追加，中文由此上屏；
//! - **没有输入法**时退回按键：evdev 码 → 可打印字符（US 布局），覆盖常用字符；
//! - 编辑态只在会话内存在，提交后变成一个普通 `Kind::Text` 对象（渲染与导出都走 core）。
//!
//! 编辑态**不写历史**：提交成功才算一次可撤销编辑（与 Windows 版一致）。

use airsnip_core::draw::text::TextSink;
use airsnip_core::editor::{DEFAULT_FONT_SIZE, Kind, Object, Point, Style};

/// 默认字体族（Linux）：Noto CJK 覆盖中英文；系统没装时 cosmic-text 会走回退链。
pub const DEFAULT_FAMILY: &str = "Noto Sans CJK SC";

use super::state::Overlay;

/// 正在输入的文本。
#[derive(Clone, Debug)]
pub struct TextEdit {
    /// 文字左上角（画布坐标）。
    pub pos: (f32, f32),
    pub text: String,
    pub font_size: f32,
    pub family: std::sync::Arc<str>,
    pub color: [u8; 4],
}

/// 点击放置（在选区内钳一次）。
pub fn begin(ov: &mut Overlay, p: (i32, i32)) {
    let Some(sel) = ov.sel else { return };
    let (x, y) = sel.clamp_point(p.0, p.1);
    ov.text_edit = Some(TextEdit {
        pos: (x as f32, y as f32),
        text: String::new(),
        font_size: DEFAULT_FONT_SIZE,
        family: std::sync::Arc::from(DEFAULT_FAMILY),
        color: ov.style.color,
    });
    ov.ime_active = false;
    crate::wayland::debug(format!("文本：开始在 ({x}, {y}) 输入"));
}

/// 追加一段文字（输入法 commit 或按键）。
pub fn insert(ov: &mut Overlay, s: &str) {
    if let Some(edit) = ov.text_edit.as_mut() {
        edit.text.push_str(s);
    }
}

/// 退格。
pub fn backspace(ov: &mut Overlay) {
    if let Some(edit) = ov.text_edit.as_mut() {
        edit.text.pop();
    }
}

/// 提交：非空则成为一个 `Kind::Text` 对象（一次可撤销编辑）。
pub fn commit(ov: &mut Overlay) -> bool {
    let Some(edit) = ov.text_edit.take() else {
        return false;
    };
    ov.ime_active = false;
    if edit.text.trim().is_empty() {
        return false;
    }
    let pos = Point::new(edit.pos.0, edit.pos.1);
    // 先量一次尺寸缓存进对象（渲染层据此算包围盒/命中，与 Windows 版同一约定）
    let spec = airsnip_core::draw::text::TextSpec {
        family: edit.family.clone(),
        size: edit.font_size,
        bold: false,
        italic: false,
    };
    let size = crate::draw::text::with_engine(|sink| sink.measure(&edit.text, &spec));
    ov.doc.begin();
    ov.doc.push(Object {
        kind: Kind::Text {
            pos,
            text: std::sync::Arc::from(edit.text.as_str()),
            font_size: edit.font_size,
            font_family: edit.family,
            bold: false,
            italic: false,
            size,
        },
        style: Style::new(edit.color, ov.style.width),
    });
    ov.doc.commit(true);
    true
}

/// 取消（Esc）：丢弃编辑内容（当前 Esc 是"提交"，保留此函数供双击重置等场景）。
#[allow(dead_code)]
pub fn cancel(ov: &mut Overlay) {
    ov.text_edit = None;
    ov.ime_active = false;
}

/// evdev 键码 → 可打印字符（US 布局，无输入法时的兜底）。
///
/// 只覆盖打字最常用的字符；带 `shift` 时给大写与符号。返回 `None` 表示不是可打印键。
pub fn key_char(code: u32, shift: bool) -> Option<char> {
    let (lo, hi) = match code {
        // 1..=10 是 Esc..9 的键位，这里只取数字行的上排数字
        2..=10 => ('1', '9'),
        11 => ('0', '0'),
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        57 => (' ', ' '),
        // 常用符号（未按 shift / 按 shift）
        12 => ('-', '_'),
        13 => ('=', '+'),
        26 => ('[', '{'),
        27 => (']', '}'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        _ => return None,
    };
    let c = if shift { hi } else { lo };
    // 数字键在 shift 下应是符号；上面数字行没做映射，交给输入法/后续完善
    Some(c)
}

/// 编辑态的文字与光标（画在 display 上，位于所有对象之上）。
pub fn paint(pixmap: &mut tiny_skia::Pixmap, edit: &TextEdit, width: f32) {
    let spec = airsnip_core::draw::text::TextSpec {
        family: edit.family.clone(),
        size: edit.font_size,
        bold: false,
        italic: false,
    };
    let pos = Point::new(edit.pos.0, edit.pos.1);
    crate::draw::text::with_engine(|sink| {
        if edit.text.is_empty() {
            // 空内容：画一条竖线光标提示"可以打字了"
            draw_caret(pixmap, edit.pos, edit.font_size);
            return;
        }
        sink.draw(
            pixmap,
            pos,
            &edit.text,
            &spec,
            edit.color,
            tiny_skia::Transform::identity(),
        );
        let (tw, _) = sink.measure(&edit.text, &spec);
        draw_caret(pixmap, (edit.pos.0 + tw, edit.pos.1), edit.font_size);
    });
    let _ = width;
}

fn draw_caret(pixmap: &mut tiny_skia::Pixmap, at: (f32, f32), font_size: f32) {
    let Some(rect) = tiny_skia::Rect::from_xywh(at.0, at.1, 2.0, font_size) else {
        return;
    };
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(26, 115, 232, 255);
    pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
}

/// 请求输入法接管（覆盖层获得键盘焦点后调用）。
pub fn enable_ime(state: &mut crate::wayland::Backend) {
    if let Some(ti) = state.text_input.clone() {
        ti.enable();
        // 光标矩形给输入法定位候选框用；覆盖层是全屏，给 0 尺寸即可（候选框跟鼠标）
        ti.set_cursor_rectangle(0, 0, 0, 0);
        ti.commit();
        crate::wayland::debug("文本：已请求输入法");
    }
}

/// 释放输入法（提交/取消后调用）。
pub fn disable_ime(state: &mut crate::wayland::Backend) {
    if let Some(ti) = state.text_input.clone() {
        ti.disable();
        ti.commit();
    }
}

impl wayland_client::Dispatch<wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3, ()>
    for crate::wayland::Backend
{
    fn event(
        state: &mut Self,
        _proxy: &wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3,
        event: wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &wayland_client::QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event;
        let mut changed = false;
        {
            let Some(ov) = state.overlay.as_mut() else {
                return;
            };
            match event {
                Event::CommitString { text } => {
                    // 协议里 text 是 Option<String>（None = 清空提交）
                    if let Some(s) = text.filter(|s| !s.is_empty()) {
                        ov.ime_active = true;
                        insert(ov, &s);
                        changed = true;
                        crate::wayland::debug(format!("文本：输入法上屏 {s:?}"));
                    }
                }
                Event::PreeditString { text, .. } => {
                    // 预编辑串只用来判断「输入法已接管」，不渲染（简化实现）
                    if text.map(|s| !s.is_empty()).unwrap_or(false) {
                        ov.ime_active = true;
                    }
                }
                Event::DeleteSurroundingText { before_length, .. } => {
                    for _ in 0..before_length.min(64) {
                        backspace(ov);
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            super::paint::refresh_all(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::state::{Overlay, Sel};

    fn overlay() -> Overlay {
        let canvas = crate::canvas::Canvas::new(400, 300);
        let mut ov = Overlay::new(&canvas).unwrap();
        ov.sel = Some(Sel {
            x: 10,
            y: 10,
            w: 300,
            h: 200,
        });
        ov
    }

    #[test]
    fn begin_clamps_into_selection() {
        let mut ov = overlay();
        begin(&mut ov, (9999, 9999));
        let e = ov.text_edit.as_ref().unwrap();
        assert_eq!(e.pos, (309.0, 209.0));
    }

    #[test]
    fn commit_creates_text_object() {
        let mut ov = overlay();
        begin(&mut ov, (20, 20));
        insert(&mut ov, "hello");
        assert!(commit(&mut ov));
        assert_eq!(ov.doc.objects().len(), 1);
        assert!(matches!(ov.doc.objects()[0].kind, Kind::Text { .. }));
        assert!(ov.text_edit.is_none());
    }

    #[test]
    fn empty_commit_is_dropped() {
        let mut ov = overlay();
        begin(&mut ov, (20, 20));
        assert!(!commit(&mut ov));
        assert!(ov.doc.objects().is_empty());
    }

    #[test]
    fn backspace_and_cancel() {
        let mut ov = overlay();
        begin(&mut ov, (20, 20));
        insert(&mut ov, "ab");
        backspace(&mut ov);
        assert_eq!(ov.text_edit.as_ref().unwrap().text, "a");
        cancel(&mut ov);
        assert!(ov.text_edit.is_none());
        assert!(ov.doc.objects().is_empty());
    }

    #[test]
    fn key_map_covers_letters_and_shift() {
        assert_eq!(key_char(30, false), Some('a'));
        assert_eq!(key_char(30, true), Some('A'));
        assert_eq!(key_char(57, false), Some(' '));
        assert_eq!(key_char(1, false), None);
    }
}
