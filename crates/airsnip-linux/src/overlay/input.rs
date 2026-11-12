// SPDX-License-Identifier: GPL-3.0-only
//! 输入：指针（框选 / 画标注）与键盘（工具切换、撤销重做、输出）。
//!
//! **不解析 xkb keymap**：Wayland 的 `wl_keyboard.key` 是 Linux evdev 原始码
//! （比 X11 keycode 小 8），数字键与 Esc/Enter/Ctrl 这些码是稳定的，直接比常量。
//! 中文输入法（text-input-v3）在文本工具那一步再接。

use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use airsnip_core::editor::Tool;

use super::state::{Action, Sel};
use super::{edit, objedit, paint};
use crate::wayland::Backend;

// Linux evdev 键码（input-event-codes.h）
const KEY_ESC: u32 = 1;
const KEY_ENTER: u32 = 28;
const KEY_CTRL_L: u32 = 29;
const KEY_S: u32 = 31;
const KEY_C: u32 = 46;
const KEY_Z: u32 = 44;
const KEY_Y: u32 = 21;
const KEY_KPENTER: u32 = 96;
const KEY_CTRL_R: u32 = 97;
const KEY_SHIFT_L: u32 = 42;
const KEY_SHIFT_R: u32 = 54;
const KEY_BACKSPACE: u32 = 14;
const KEY_DELETE: u32 = 111;
const KEY_F3: u32 = 61;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
/// BTN_LEFT
const BTN_LEFT: u32 = 0x110;

impl Dispatch<WlSeat, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlPointer, ()> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let cursor_dev = state.cursor_shape_device.clone();
        let mut repaint = false;
        let mut need_ime_on = false;
        let mut need_ime_off = false;
        {
            let Some(ov) = state.overlay.as_mut() else {
                return;
            };
            match event {
                wl_pointer::Event::Enter {
                    serial,
                    surface,
                    surface_x,
                    surface_y,
                } => {
                    ov.serial = serial;
                    ov.active = ov.layers.iter().position(|l| l.surface == surface);
                    if let Some(i) = ov.active {
                        ov.pointer = Some(ov.layers[i].to_canvas(surface_x, surface_y));
                        let g = ov.layers[i].geom;
                        crate::wayland::debug(format!(
                            "指针进入：surface=({surface_x:.0},{surface_y:.0}) 图层{i} phys={:?} scale={} → 画布{:?}",
                            g.phys,
                            g.scale(),
                            ov.pointer
                        ));
                    }
                    if let Some(dev) = cursor_dev {
                        use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;
                        dev.set_shape(serial, Shape::Crosshair);
                    }
                }
                wl_pointer::Event::Motion {
                    surface_x, surface_y, ..
                } => {
                    if let Some(i) = ov.active {
                        let p = ov.layers[i].to_canvas(surface_x, surface_y);
                        ov.pointer = Some(p);
                        if ov.edit.is_some() {
                            objedit::drag(ov, p);
                            repaint = true;
                        } else if ov.drawing {
                            edit::update(ov, p, ov.shift);
                            repaint = true;
                        } else if let Some(from) = ov.drag_from {
                            let (cw, ch) = (ov.bright.width() as i32, ov.bright.height() as i32);
                            ov.sel = Some(Sel::from_drag(from, p).clamped(cw, ch));
                            repaint = true;
                        } else if ov.current_sel().is_some() {
                            // 悬停反馈（工具栏按钮底色）
                            repaint = true;
                        }
                    }
                }
                wl_pointer::Event::Button {
                    serial,
                    button,
                    state: bs,
                    ..
                } => {
                    ov.serial = serial;
                    if button != BTN_LEFT {
                        return;
                    }
                    let pressed = matches!(bs, WEnum::Value(ButtonState::Pressed));
                    let Some(p) = ov.pointer else { return };
                    // 正在输入文本：任何点击都先提交（点哪儿都算"写完了"）
                    if pressed && ov.text_edit.is_some() {
                        super::text_input::commit(ov);
                        need_ime_off = true;
                    }
                    // 1) 工具栏优先：按在工具栏上就只处理按钮，不动选区/标注
                    let mut handled = false;
                    if pressed && let Some(sel) = ov.current_sel() {
                        let (cw, ch) = (ov.bright.width() as i32, ov.bright.height() as i32);
                        let tb = super::toolbar::layout(cw, ch, sel);
                        if let Some(item) = tb.hit(p.0, p.1) {
                            handled = true;
                            ov.toolbar_hold = true;
                            on_toolbar(ov, item);
                        }
                    }
                    if pressed {
                        // 2) 对象编辑优先：点中对象 = 选中（Ctrl 才是拖动），点手柄 = 缩放
                        if !handled && ov.current_sel().is_some() {
                            handled = objedit::press(ov, p, ov.ctrl);
                        }
                        // 3) 已框选且按在选区内 → 画标注；否则开始新的框选
                        if !handled {
                            let inside = ov.current_sel().map(|s| s.contains(p.0, p.1)).unwrap_or(false);
                            if inside {
                                // 压在对象上：先记下"可能是单击选中"（松手时按有无位移判定）
                                if !ov.ctrl {
                                    if let Some(idx) = objedit::hit_object_loose(
                                        ov,
                                        airsnip_core::editor::Point::new(p.0 as f32, p.1 as f32),
                                    ) {
                                        ov.pending_select = Some((idx, ov.doc.objects().len()));
                                    }
                                }
                                ov.drawing = true;
                                edit::begin(ov, p);
                                if ov.text_edit.is_some() {
                                    need_ime_on = true;
                                }
                            } else {
                                ov.drawing = false;
                                ov.drag_from = Some(p);
                                ov.sel = None;
                                crate::wayland::debug(format!("框选起点 {p:?}"));
                                ov.doc = airsnip_core::editor::Document::new();
                            }
                        }
                    } else if ov.toolbar_hold {
                        // 工具栏上松手：不结束任何手势
                        ov.toolbar_hold = false;
                    } else if ov.edit.is_some() {
                        objedit::release(ov);
                    } else if ov.drawing {
                        edit::commit(ov);
                        ov.drawing = false;
                        // 单击（没有真的画出对象）= 选中按下时压着的那个对象
                        if let Some((idx, len)) = ov.pending_select.take()
                            && ov.doc.objects().len() == len
                        {
                            ov.selected = Some(idx);
                            crate::wayland::debug(format!("选中对象 #{idx}"));
                        }
                    } else {
                        ov.drag_from = None;
                        if ov.sel.map(|s| !s.usable()).unwrap_or(true) {
                            ov.sel = None;
                        }
                        crate::wayland::debug(format!("框选结束 {:?}", ov.sel));
                    }
                    repaint = true;
                }
                wl_pointer::Event::Axis { axis, value, .. } => {
                    // 只在垂直滚动时调线宽（数值方向：向下为负增长）
                    let vertical = matches!(
                        axis,
                        WEnum::Value(wayland_client::protocol::wl_pointer::Axis::VerticalScroll)
                    );
                    if vertical && value.abs() > 0.01 {
                        let delta = if value > 0.0 { -1.0 } else { 1.0 };
                        objedit::adjust_width(ov, delta);
                        repaint = true;
                    }
                }
                wl_pointer::Event::Leave { .. } => ov.active = None,
                _ => {}
            }
        }
        if need_ime_off {
            super::text_input::disable_ime(state);
        }
        if need_ime_on {
            super::text_input::enable_ime(state);
        }
        if repaint {
            paint::refresh_all(state);
        }
    }
}

/// 工具栏点击。
fn on_toolbar(ov: &mut super::state::Overlay, item: super::toolbar::Item) {
    use super::toolbar::Item;
    use airsnip_core::editor::DEFAULT_COLORS;
    match item {
        Item::Tool(t) => {
            ov.tool = t;
            ov.draft = None;
        }
        Item::Color(i) => {
            if i < DEFAULT_COLORS.len() {
                ov.color_index = i;
                ov.style.color = DEFAULT_COLORS[i];
                // 有选中对象时改它的颜色（一步撤销）
                let c = DEFAULT_COLORS[i];
                super::objedit::restyle_selected(ov, |obj| obj.style.color = c);
            }
        }
        Item::Fill => {
            ov.filled = !ov.filled;
            let filled = ov.filled;
            super::objedit::restyle_selected(ov, |obj| match &mut obj.kind {
                airsnip_core::editor::Kind::Rect { filled: f, .. }
                | airsnip_core::editor::Kind::Ellipse { filled: f, .. } => *f = filled,
                _ => {}
            });
        }
        Item::Undo => {
            edit::undo(ov);
        }
        Item::Redo => {
            edit::redo(ov);
        }
        Item::Pin => ov.finish(Action::Pin),
        Item::Copy => ov.finish(Action::Copy),
        Item::Save => ov.finish(Action::Save),
        Item::Sep => {}
    }
}

impl Dispatch<WlKeyboard, ()> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut repaint = false;
        let mut need_ime_off = false;
        let mut need_cursor_sync = false;
        {
            let Some(ov) = state.overlay.as_mut() else {
                return;
            };
            match event {
                wl_keyboard::Event::Enter { serial, surface, .. } => {
                    ov.serial = serial;
                    // 记住键盘焦点在哪块图层：输入法候选框要用该表面的局部坐标
                    ov.kb_layer = ov.layers.iter().position(|l| l.surface == surface);
                    need_cursor_sync = true;
                }
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    ov.ctrl = mods_depressed & 4 != 0; // XKB Control
                    ov.shift = mods_depressed & 1 != 0; // XKB Shift
                }
                wl_keyboard::Event::Key {
                    serial, key, state: ks, ..
                } => {
                    ov.serial = serial;
                    if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                        return;
                    }
                    crate::wayland::debug(format!(
                        "按键 evdev={key} ctrl={} shift={} 文本态={} ime={}",
                        ov.ctrl,
                        ov.shift,
                        ov.text_edit.is_some(),
                        ov.ime_active
                    ));
                    // 文本编辑态：按键只服务于输入，不切工具、不结束会话
                    let editing = ov.text_edit.is_some();
                    if editing {
                        match key {
                            // Esc 结束输入并**保留**已输入内容（不丢字）。
                            // 不用 Esc 丢弃：输入法会把 Enter 吃掉（只回 commit_string），
                            // 若 Esc 也丢弃，用户就没有"结束但不丢字"的键可用了。
                            KEY_ESC => {
                                let ok = super::text_input::commit(ov);
                                crate::wayland::debug(format!("文本：Esc 结束 → 提交 {ok}"));
                                need_ime_off = true;
                            }
                            // Enter 只在没有输入法接管时作为提交键：
                            // 有输入法时 Enter 会被它吃掉（用于确认候选词），我们根本收不到。
                            KEY_ENTER | KEY_KPENTER => {
                                let ok = super::text_input::commit(ov);
                                crate::wayland::debug(format!("文本：Enter 提交 → {ok}"));
                                need_ime_off = true;
                            }
                            KEY_BACKSPACE => {
                                super::text_input::backspace(ov);
                                need_cursor_sync = true;
                            }
                            KEY_SHIFT_L | KEY_SHIFT_R => {}
                            _ => {
                                // 按键兜底：**只要收到了按键就说明输入法没在接管**
                                // （输入法接管时合成器不会把按键发给我们，只回 commit_string）。
                                // 早期用 `ime_active` 当闸门，导致"先用中文、再切回英文"后按键被
                                // 全丢：那个标志一旦置上就再没复位。
                                if let Some(c) = super::text_input::key_char(key, ov.shift) {
                                    let mut buf = [0u8; 4];
                                    super::text_input::insert(ov, c.encode_utf8(&mut buf));
                                    need_cursor_sync = true;
                                }
                            }
                        }
                        repaint = true;
                    } else {
                        match key {
                            KEY_CTRL_L | KEY_CTRL_R => ov.ctrl = true,
                            KEY_ESC => ov.finish(Action::Cancel),
                            KEY_ENTER | KEY_KPENTER => ov.finish(Action::Copy),
                            // F3：贴图（与 Windows 版同一个键）
                            KEY_F3 => ov.finish(Action::Pin),
                            KEY_C if ov.ctrl => ov.finish(Action::Copy),
                            KEY_S if ov.ctrl => ov.finish(Action::Save),
                            KEY_BACKSPACE | KEY_DELETE => {
                                if objedit::delete(ov) {
                                    repaint = true;
                                    crate::wayland::debug("删除选中对象");
                                }
                            }
                            KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => {
                                let step = if ov.shift { 5.0 } else { 1.0 };
                                let (dx, dy) = match key {
                                    KEY_LEFT => (-step, 0.0),
                                    KEY_RIGHT => (step, 0.0),
                                    KEY_UP => (0.0, -step),
                                    _ => (0.0, step),
                                };
                                if objedit::nudge(ov, dx, dy) {
                                    repaint = true;
                                }
                            }
                            KEY_Z if ov.ctrl => repaint = edit::undo(ov),
                            KEY_Y if ov.ctrl => repaint = edit::redo(ov),
                            // 工具切换：1–9、0（与 README 的 10 个工具顺序一致）
                            k if (2..=11).contains(&k) => {
                                let idx = (k - 2) as usize; // evdev 1..=10 对应键帽 1..0
                                if let Some(t) = Tool::ALL.get(idx).copied() {
                                    ov.tool = t;
                                    ov.draft = None;
                                    repaint = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if need_ime_off {
            super::text_input::disable_ime(state);
        }
        if need_cursor_sync {
            super::text_input::sync_cursor(state);
        }
        if repaint {
            paint::refresh_all(state);
        }
    }
}

// `KEY_SHIFT_L` 预留给 S3 的 Shift 约束（正方形 / 方向吸附）
#[allow(dead_code)]
const _SHIFT: u32 = KEY_SHIFT_L;

impl Dispatch<wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ()>
    for Backend
{
    fn event(
        _state: &mut Self,
        _proxy: &wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        _event: <wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, ()>
    for Backend
{
    fn event(
        _state: &mut Self,
        _proxy: &wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
        _event: <wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
