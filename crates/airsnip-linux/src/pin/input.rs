// SPDX-License-Identifier: GPL-3.0-only
//! 贴图的输入（PIN-2/PIN-4/PIN-6）：按住左键拖动（**可跨屏**）、滚轮缩放、
//! `Ctrl`+滚轮透明度、方向键微调、双击 / `Esc` 关闭、`B` 开关边框。
//!
//! 事件派发用**带标记的 udata**（`PinPtr` / `PinKbd` / `PinSurfaceKey` / `PinLayerKey`），
//! 与覆盖层那份 `Dispatch<WlPointer, ()>` 互不干扰。
//!
//! **坐标模型**：每个输出一块覆盖整屏的表面，`wl_pointer` 的局部坐标 + 该输出原点
//! = 桌面坐标。拖动位置 = `桌面指针 - 抓取偏移`，与表面位置无关 → 幂等、不漂移、能跨屏。

use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, Axis, ButtonState, WlPointer};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use super::paint;
use crate::wayland::Backend;

/// 贴图指针的 udata 标记。
pub struct PinPtr;
/// 贴图键盘的 udata 标记。
pub struct PinKbd;
/// 贴图表面的 udata 标记（记录所属输出）。
#[allow(dead_code)] // 目前不需要读下标，仅用于与覆盖层的派发区分开
pub struct PinSurfaceKey(pub usize);
/// 贴图 layer surface 的 udata 标记（记录所属输出）。
pub struct PinLayerKey(pub usize);

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

/// 拖动状态：按下瞬间「桌面指针 - 贴图左上角」，之后不变。
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

/// 拖动中的新位置（桌面坐标）：`指针 - 抓取偏移`，钳在虚拟桌面内。
///
/// `desktop` 是整块虚拟桌面 `(x0, y0, w, h)`；跨屏拖动因此是自然支持的。
pub fn drag_target(
    pointer: (f64, f64),
    offset: (f64, f64),
    desktop: (i32, i32, i32, i32),
    size: (i32, i32),
) -> (i32, i32) {
    let x = (pointer.0 - offset.0).round() as i32;
    let y = (pointer.1 - offset.1).round() as i32;
    let min_x = desktop.0;
    let min_y = desktop.1;
    let max_x = (desktop.0 + desktop.2 - size.0).max(min_x);
    let max_y = (desktop.1 + desktop.3 - size.1).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
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
                surface,
                surface_x,
                surface_y,
            } => {
                state.pin_serial = serial;
                state.pin_cursor = (surface_x, surface_y);
                // 记下指针当前在哪个输出的表面上（换算桌面坐标要用它的原点）
                state.pin_active_layer = state
                    .pins
                    .first()
                    .and_then(|p| p.layers.iter().position(|l| l.surface == surface));
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                state.pin_cursor = (surface_x, surface_y);
                let Some(pin) = state.pins.first_mut() else {
                    return;
                };
                let Some(drag) = pin.drag else { return };
                let Some(li) = state.pin_active_layer else { return };
                let Some(layer) = pin.layers.get(li) else { return };
                // 局部坐标 + 输出原点 = 桌面坐标
                let pointer = (surface_x + layer.origin.0 as f64, surface_y + layer.origin.1 as f64);
                let next = drag_target(pointer, drag.offset, pin.desktop_rect(), pin.scaled_size());
                if next != pin.pos {
                    crate::wayland::debug(format!("拖动：指针{pointer:?} → {next:?}"));
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
                        // 开始拖动：记录「桌面指针 - 贴图左上角」
                        let Some(li) = state.pin_active_layer else {
                            return;
                        };
                        if let Some(pin) = state.pins.first_mut() {
                            if let Some(layer) = pin.layers.get(li) {
                                let pointer = (
                                    state.pin_cursor.0 + layer.origin.0 as f64,
                                    state.pin_cursor.1 + layer.origin.1 as f64,
                                );
                                pin.drag = Some(Drag {
                                    offset: (pointer.0 - pin.pos.0 as f64, pointer.1 - pin.pos.1 as f64),
                                });
                            }
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
                    let next = next_opacity(pin.opacity, value < 0.0);
                    if next != pin.opacity {
                        pin.opacity = next;
                        dirty = true;
                        crate::wayland::debug(format!("贴图透明度 → {}", pin.opacity));
                    }
                } else {
                    let next = next_zoom(pin.zoom, value < 0.0);
                    if (next - pin.zoom).abs() > 0.01 {
                        let (ow, oh) = pin.scaled_size();
                        pin.zoom = next;
                        let (nw, nh) = pin.scaled_size();
                        // 以中心为基准缩放
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
                state.pin_active_layer = None;
            }
            _ => {}
        }
        if dirty {
            // 先同步输入区域（必须在旧区域把指针"甩出去"之前），再重绘
            paint::update_input(state, 0);
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
                        let desktop = pin.desktop_rect();
                        let size = pin.scaled_size();
                        let moved = (pin.pos.0 + dx, pin.pos.1 + dy);
                        pin.pos = drag_target((moved.0 as f64, moved.1 as f64), (0.0, 0.0), desktop, size);
                        dirty = true;
                    }
                    KEY_SHIFT_L | KEY_SHIFT_R => {}
                    _ => {}
                }
            }
            _ => {}
        }
        if dirty {
            paint::update_input(state, 0);
            paint::paint_all(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 虚拟桌面：左屏 0..1920，右屏 1920..3840（高度 1080）
    const DESKTOP: (i32, i32, i32, i32) = (0, 0, 3840, 1080);
    const SIZE: (i32, i32) = (400, 300);

    #[test]
    fn drag_is_idempotent() {
        let offset = (50.0, 50.0);
        let p1 = drag_target((250.0, 250.0), offset, DESKTOP, SIZE);
        assert_eq!(p1, (200, 200));
        assert_eq!(drag_target((250.0, 250.0), offset, DESKTOP, SIZE), p1);
    }

    #[test]
    fn drag_follows_pointer_exactly() {
        let offset = (50.0, 50.0);
        assert_eq!(drag_target((350.0, 290.0), offset, DESKTOP, SIZE), (300, 240));
    }

    #[test]
    fn drag_can_cross_to_second_output() {
        let offset = (50.0, 50.0);
        // 指针到右屏 x=2500 → 贴图可以放到 x=2450（超过左屏宽度 1920）
        assert_eq!(drag_target((2500.0, 400.0), offset, DESKTOP, SIZE), (2450, 350));
    }

    #[test]
    fn drag_clamps_to_desktop_edges() {
        let offset = (50.0, 50.0);
        assert_eq!(drag_target((0.0, 0.0), offset, DESKTOP, SIZE), (0, 0));
        assert_eq!(drag_target((9999.0, 9999.0), offset, DESKTOP, SIZE), (3440, 780));
    }

    #[test]
    fn opacity_steps_and_clamps() {
        assert_eq!(next_opacity(255, false), 239);
        assert_eq!(next_opacity(48, false), 32);
        assert_eq!(next_opacity(200, true), 216);
        assert_eq!(next_opacity(255, true), 255);
    }

    #[test]
    fn zoom_steps_and_clamps() {
        assert!((next_zoom(1.0, true) - 1.1).abs() < 1e-6);
        assert!((next_zoom(1.0, false) - 0.9).abs() < 1e-6);
        assert_eq!(next_zoom(5.0, true), 5.0);
        assert_eq!(next_zoom(0.2, false), 0.2);
    }
}
