// SPDX-License-Identifier: GPL-3.0-only
//! 贴图的输入（PIN-2/PIN-4/PIN-6）：按住左键拖动、滚轮缩放、`Ctrl`+滚轮透明度、
//! 方向键微调、双击 / `Esc` 关闭、`B` 开关边框。
//!
//! 事件派发用**带标记的 udata**（`PinPtr` / `PinKbd` / `PinSurface` / `PinLayer`），
//! 与覆盖层那份 `Dispatch<WlPointer, ()>` 互不干扰。
//!
//! **坐标模型**：贴图表面覆盖整个输出，`wl_pointer` 的局部坐标因此就是输出内绝对坐标，
//! 与贴图自己在哪里无关。拖动位置 = `指针 - 按下时的抓取偏移`，幂等、不累积漂移。
//! （早期版本用 `set_margin` 移动"刚好包住图像"的小表面：表面移动会让指针局部坐标跟着变，
//! 而合成器应用 margin 是异步的，两者互相追着跑 —— 表现为"不跟手"或"拖一下就飞出屏幕"。）

use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, Axis, ButtonState, WlPointer};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use super::paint;
use crate::wayland::Backend;

/// 贴图指针的 udata 标记。
pub struct PinPtr;
/// 贴图键盘的 udata 标记。
pub struct PinKbd;
/// 贴图表面的 udata 标记。
pub struct PinSurface;
/// 贴图 layer surface 的 udata 标记。
pub struct PinLayer;

// Linux evdev 键码
const KEY_ESC: u32 = 1;
const KEY_B: u32 = 48;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_SHIFT_L: u32 = 42;
const KEY_SHIFT_R: u32 = 54;
const BTN_LEFT: u32 = 0x110;

/// 拖动状态：按下瞬间「指针 - 贴图左上角」的偏移，之后保持不变。
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub offset: (f64, f64),
}

/// 滚轮 → 新透明度（步进 16，夹在 32..=255；向下滚变淡）。
pub fn next_opacity(cur: u8, up: bool) -> u8 {
    let step = if up { 16 } else { -16 };
    (cur as i32 + step).clamp(32, 255) as u8
}

/// 滚轮 → 新缩放（每次 10%，夹在 0.2..=5.0；向上滚放大）。
pub fn next_zoom(cur: f32, up: bool) -> f32 {
    let factor = if up { 1.1 } else { 0.9 };
    (cur * factor).clamp(0.2, 5.0)
}

/// 拖动中的新位置：`指针绝对坐标 - 抓取偏移`，钳在输出内。
///
/// 表面覆盖整屏 → `local` 就是绝对坐标 → 这个式子与表面当前位置无关，
/// 因此**幂等**：同一指针位置重复计算得到同一结果，不会因事件频率/合成器延迟而漂移。
pub fn drag_target(local: (f64, f64), offset: (f64, f64), surface: (u32, u32), size: (i32, i32)) -> (i32, i32) {
    let max_x = (surface.0 as i32 - size.0).max(0);
    let max_y = (surface.1 as i32 - size.1).max(0);
    let x = (local.0 - offset.0).round() as i32;
    let y = (local.1 - offset.1).round() as i32;
    (x.clamp(0, max_x), y.clamp(0, max_y))
}

impl Dispatch<WlPointer, PinPtr> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &PinPtr,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut dirty = false;
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pin_serial = serial;
                state.pin_cursor = (surface_x, surface_y);
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                state.pin_cursor = (surface_x, surface_y);
                let Some(pin) = state.pins.first_mut() else {
                    return;
                };
                // 只有**按住左键**时才移动
                let Some(drag) = pin.drag else { return };
                let size = pin.scaled_size();
                let surface = pin.logical.unwrap_or((1, 1));
                let next = drag_target((surface_x, surface_y), drag.offset, surface, size);
                if next != pin.pos {
                    crate::wayland::debug(format!(
                        "拖动：指针({surface_x:.0},{surface_y:.0}) 偏移{:?} 表面{:?} → {:?}",
                        drag.offset, surface, next
                    ));
                    pin.pos = next;
                    dirty = true;
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: bs,
                ..
            } => {
                state.pin_serial = serial;
                if button != BTN_LEFT {
                    return;
                }
                if matches!(bs, WEnum::Value(ButtonState::Pressed)) {
                    // 先判断双击（400ms 内、位置差 < 6px）
                    let now = std::time::Instant::now();
                    let near = state
                        .pin_last_click
                        .map(|(t, p)| {
                            now.duration_since(t).as_millis() < 400
                                && (state.pin_cursor.0 - p.0).abs() < 6.0
                                && (state.pin_cursor.1 - p.1).abs() < 6.0
                        })
                        .unwrap_or(false);
                    if near {
                        if let Some(pin) = state.pins.first_mut() {
                            pin.closed = true;
                        }
                        state.pin_last_click = None;
                    } else {
                        state.pin_last_click = Some((now, state.pin_cursor));
                        // 开始拖动：记录"指针 - 贴图左上角"
                        if let Some(pin) = state.pins.first_mut() {
                            pin.drag = Some(Drag {
                                offset: (
                                    state.pin_cursor.0 - pin.pos.0 as f64,
                                    state.pin_cursor.1 - pin.pos.1 as f64,
                                ),
                            });
                        }
                    }
                } else if let Some(pin) = state.pins.first_mut() {
                    pin.drag = None; // 松手结束拖动
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let vertical = matches!(axis, WEnum::Value(Axis::VerticalScroll));
                if !vertical || value.abs() < 0.01 {
                    return;
                }
                let Some(pin) = state.pins.first_mut() else {
                    return;
                };
                if state.pin_ctrl {
                    // Ctrl + 滚轮：透明度
                    let next = next_opacity(pin.opacity, value < 0.0);
                    if next != pin.opacity {
                        pin.opacity = next;
                        dirty = true;
                        crate::wayland::debug(format!("贴图透明度 → {}", pin.opacity));
                    }
                } else {
                    // 滚轮：缩放（0.2× – 5×），保持贴图中心不动
                    let next = next_zoom(pin.zoom, value < 0.0);
                    if (next - pin.zoom).abs() > 0.01 {
                        let (ow, oh) = pin.scaled_size();
                        pin.zoom = next;
                        let (nw, nh) = pin.scaled_size();
                        pin.pos = ((pin.pos.0 + (ow - nw) / 2).max(0), (pin.pos.1 + (oh - nh) / 2).max(0));
                        dirty = true;
                        crate::wayland::debug(format!("贴图缩放 → {:.2}× @{:?}", pin.zoom, pin.pos));
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                // 离开表面：结束拖动（否则下次进入会"接着拖"）
                if let Some(pin) = state.pins.first_mut() {
                    pin.drag = None;
                }
            }
            _ => {}
        }
        if dirty {
            paint::paint_all(state);
        }
    }
}

impl Dispatch<WlKeyboard, PinKbd> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &PinKbd,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut dirty = false;
        match event {
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                state.pin_ctrl = mods_depressed & 4 != 0;
                state.pin_shift = mods_depressed & 1 != 0;
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                    return;
                }
                let shift = state.pin_shift;
                let Some(pin) = state.pins.first_mut() else {
                    return;
                };
                match key {
                    KEY_ESC => {
                        pin.closed = true;
                        crate::wayland::debug("贴图：Esc 关闭");
                    }
                    KEY_B => {
                        pin.border = !pin.border;
                        dirty = true;
                        crate::wayland::debug(format!("贴图边框 → {}", pin.border));
                    }
                    KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => {
                        let step = if shift { 10 } else { 1 };
                        let (dx, dy) = match key {
                            KEY_LEFT => (-step, 0),
                            KEY_RIGHT => (step, 0),
                            KEY_UP => (0, -step),
                            _ => (0, step),
                        };
                        pin.pos = ((pin.pos.0 + dx).max(0), (pin.pos.1 + dy).max(0));
                        dirty = true;
                    }
                    KEY_SHIFT_L | KEY_SHIFT_R => {}
                    _ => {}
                }
            }
            _ => {}
        }
        if dirty {
            paint::paint_all(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_is_idempotent() {
        // 表面 1920×1080，贴图 400×300，按下点相对左上角 (50,50)
        let offset = (50.0, 50.0);
        let p1 = drag_target((250.0, 250.0), offset, (1920, 1080), (400, 300));
        assert_eq!(p1, (200, 200));
        // 同一指针位置重复计算 → 同一结果（不因事件频率漂移）
        assert_eq!(drag_target((250.0, 250.0), offset, (1920, 1080), (400, 300)), p1);
    }

    #[test]
    fn drag_follows_pointer_exactly() {
        let offset = (50.0, 50.0);
        // 指针右移 100、下移 40 → 位置同样移动 100/40
        assert_eq!(
            drag_target((350.0, 290.0), offset, (1920, 1080), (400, 300)),
            (300, 240)
        );
    }

    #[test]
    fn opacity_steps_and_clamps() {
        assert_eq!(next_opacity(255, false), 239);
        assert_eq!(next_opacity(48, false), 32, "下限 32");
        assert_eq!(next_opacity(200, true), 216);
        assert_eq!(next_opacity(255, true), 255, "上限 255");
    }

    #[test]
    fn zoom_steps_and_clamps() {
        assert!((next_zoom(1.0, true) - 1.1).abs() < 1e-6);
        assert!((next_zoom(1.0, false) - 0.9).abs() < 1e-6);
        assert_eq!(next_zoom(5.0, true), 5.0);
        assert!((next_zoom(0.25, false) - 0.225).abs() < 1e-6);
        assert_eq!(next_zoom(0.2, false), 0.2);
    }

    #[test]
    fn drag_clamps_inside_output() {
        let offset = (50.0, 50.0);
        assert_eq!(drag_target((0.0, 0.0), offset, (1920, 1080), (400, 300)), (0, 0));
        assert_eq!(
            drag_target((5000.0, 5000.0), offset, (1920, 1080), (400, 300)),
            (1520, 780)
        );
    }
}
