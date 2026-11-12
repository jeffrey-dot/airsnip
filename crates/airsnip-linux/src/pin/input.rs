// SPDX-License-Identifier: GPL-3.0-only
//! 贴图的输入（PIN-2/PIN-4/PIN-6）：拖动、滚轮缩放、`Ctrl`+滚轮透明度、方向键微调、
//! 双击 / `Esc` 关闭、`B` 开关边框。
//!
//! 事件派发用**带标记的 udata**（`PinPtr` / `PinKbd`）与覆盖层区分开：
//! 同一个 `Backend` 上可以有多套 `Dispatch<WlPointer, _>` 实现，只要 udata 类型不同。
//! 贴图自己新建 pointer / keyboard 对象（`seat.get_pointer(qh, PinPtr)`），
//! 因此覆盖层那份 `Dispatch<WlPointer, ()>` 一行都不用动。

use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, Axis, ButtonState, WlPointer};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use super::paint;
use crate::wayland::Backend;

/// 贴图指针的 udata 标记。
pub struct PinPtr;
/// 贴图表面的 udata 标记。
pub struct PinSurface;
/// 贴图 layer surface 的 udata 标记。
pub struct PinLayer;
/// 贴图键盘的 udata 标记。
pub struct PinKbd;

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

/// 拖动状态：抓住时指针在表面内的位置 + 当时的绝对位置。
/// 绝对位置 = 表面位置 + 局部坐标；拖动时用它反推目标位置，
/// 避免"表面移动 → 局部坐标变化 → 再移动"造成的抖动。
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub grab: (f64, f64),
}

impl Backend {
    fn pin_index_under(&self, x: f64, y: f64) -> Option<usize> {
        // 一个进程只有一块贴图，直接取第 0 块；多贴图时按局部坐标判断
        let pin = self.pins.first()?;
        let (w, h) = pin.scaled_size();
        if x >= 0.0 && y >= 0.0 && x < w as f64 && y < h as f64 {
            Some(0)
        } else {
            None
        }
    }
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
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pin_serial = serial;
                if let Some(pin) = state.pins.first_mut() {
                    pin.drag = Some(Drag {
                        grab: (surface_x, surface_y),
                    });
                }
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                let Some(pin) = state.pins.first_mut() else {
                    return;
                };
                let Some(drag) = pin.drag else { return };
                // 目标位置 = (当前表面位置 + 局部坐标) - 抓取点
                let target_x = pin.pos.0 as f64 + surface_x - drag.grab.0;
                let target_y = pin.pos.1 as f64 + surface_y - drag.grab.1;
                let nx = (target_x.round() as i32).max(0);
                let ny = (target_y.round() as i32).max(0);
                if (nx, ny) != pin.pos {
                    pin.pos = (nx, ny);
                    pin.apply_pos();
                    crate::wayland::debug(format!("贴图位置 → {:?}", pin.pos));
                }
                // 贴图跟着指针走，局部坐标应保持稳定；这里不更新 grab
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
                    // 双击关闭（400ms 内、位置差 < 6px）
                    let now = std::time::Instant::now();
                    let near = state
                        .pin_last_click
                        .map(|(t, p)| {
                            now.duration_since(t).as_millis() < 400 && {
                                let (x, y) = state.pin_cursor;
                                (x - p.0).abs() < 6.0 && (y - p.1).abs() < 6.0
                            }
                        })
                        .unwrap_or(false);
                    if near {
                        if let Some(pin) = state.pins.first_mut() {
                            pin.closed = true;
                        }
                        state.pin_last_click = None;
                    } else {
                        state.pin_last_click = Some((now, state.pin_cursor));
                    }
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
                    let step = if value > 0.0 { -16 } else { 16 };
                    let next = (pin.opacity as i32 + step).clamp(32, 255) as u8;
                    if next != pin.opacity {
                        pin.opacity = next;
                        crate::wayland::debug(format!("贴图透明度 → {}", pin.opacity));
                    }
                } else {
                    // 滚轮：缩放（0.2× – 5×），重置缓冲后等 configure 重建
                    let factor = if value > 0.0 { 0.9 } else { 1.1 };
                    let next = (pin.zoom * factor).clamp(0.2, 5.0);
                    if (next - pin.zoom).abs() > 0.01 {
                        pin.zoom = next;
                        pin.reset_buffers();
                        pin.apply_zoom();
                        crate::wayland::debug(format!("贴图缩放 → {:.2}×", pin.zoom));
                    }
                }
            }
            _ => {}
        }
        paint::paint_all(state);
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
        let mut repaint = false;
        match event {
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                state.pin_ctrl = mods_depressed & 4 != 0;
                state.pin_shift = mods_depressed & 1 != 0;
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                    return;
                }
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
                        repaint = true;
                        crate::wayland::debug(format!("贴图边框 → {}", pin.border));
                    }
                    KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => {
                        let step = if state.pin_shift { 10 } else { 1 };
                        let (dx, dy) = match key {
                            KEY_LEFT => (-step, 0),
                            KEY_RIGHT => (step, 0),
                            KEY_UP => (0, -step),
                            _ => (0, step),
                        };
                        pin.pos = ((pin.pos.0 + dx).max(0), (pin.pos.1 + dy).max(0));
                        pin.apply_pos();
                    }
                    KEY_SHIFT_L | KEY_SHIFT_R => {}
                    _ => {}
                }
            }
            _ => {}
        }
        if repaint {
            paint::paint_all(state);
        }
    }
}
