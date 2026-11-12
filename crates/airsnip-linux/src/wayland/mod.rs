// SPDX-License-Identifier: GPL-3.0-only
//! Wayland 连接与全局对象绑定（ADR 0034 阶段 1）。
//!
//! 与 Windows 版对应关系的差别：Wayland **没有**「整个虚拟桌面的一张位图」这种概念，
//! 抓屏是逐个 `wl_output` 走 `zwlr_screencopy_manager_v1`，再由本模块把这些画面按
//! 输出的逻辑位置合成一张画布（见 `capture::capture_all`）。
//!
//! 事件循环：阶段 1 只需「发请求 → 等结果」，用 `EventQueue::roundtrip` /
//! `blocking_dispatch` 就够；阶段 2 起（覆盖层）需换成 `calloop`，届时再拆。

pub mod output;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::Error;
use crate::capture;
use output::Info;

/// `AIRSNIP_DEBUG=1` 时打印排障日志（与 Windows 版同名环境变量）。
pub fn debug(msg: impl std::fmt::Display) {
    if std::env::var_os("AIRSNIP_DEBUG").is_some() {
        eprintln!("[airsnip] {msg}");
    }
}

/// 全部绑定到的全局对象 + 已收集的输出信息 + 当前抓帧进度。
#[derive(Default)]
pub struct Backend {
    pub registry: Option<WlRegistry>,
    pub shm: Option<WlShm>,
    pub compositor: Option<WlCompositor>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub seat: Option<WlSeat>,
    pub pointer: Option<WlPointer>,
    pub keyboard: Option<WlKeyboard>,
    pub data_device_manager: Option<WlDataDeviceManager>,
    pub data_device: Option<WlDataDevice>,
    pub cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    pub cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    pub text_input_manager: Option<ZwpTextInputManagerV3>,
    pub text_input: Option<ZwpTextInputV3>,
    pub screencopy: Option<ZwlrScreencopyManagerV1>,
    pub xdg_output_manager: Option<ZxdgOutputManagerV1>,
    pub outputs: Vec<Info>,
    pub frame: capture::Progress,
    /// 覆盖层会话状态（只在 `snip` 子命令下存在）。
    pub overlay: Option<crate::overlay::state::Overlay>,
    /// 贴图（`pin` 子命令）：一个进程一块。
    pub pins: Vec<crate::pin::Pin>,
    /// 贴图用的输入状态（与覆盖层分开，靠 udata 标记区分派发）。
    pub pin_serial: u32,
    pub pin_cursor: (f64, f64),
    pub pin_last_click: Option<(std::time::Instant, (f64, f64))>,
    pub pin_ctrl: bool,
    pub pin_shift: bool,
    pub pin_pointer: Option<WlPointer>,
    pub pin_keyboard: Option<WlKeyboard>,
}

/// 连接 = 后端状态 + 事件队列（二者分开存放，避免 `dispatch` 时借用冲突）。
pub struct Session {
    pub backend: Backend,
    pub queue: EventQueue<Backend>,
    pub qh: QueueHandle<Backend>,
}

impl Session {
    /// 连上合成器并把需要的全局对象绑定好，随后等首轮事件把输出信息填满。
    ///
    /// 不用 `registry_queue_init`：它的首轮 globals 只会进 `GlobalList`、**不会**派发到
    /// 本 crate 的 `Dispatch<WlRegistry>` 实现（首轮时 `initial_roundtrip_done` 还是 false），
    /// 而我们需要按 global **名字**逐个绑定 `wl_output`（多个输出是同一接口的多个实例）。
    pub fn connect() -> Result<Self, Error> {
        let conn = Connection::connect_to_env()
            .map_err(|e| format!("连不上 Wayland 合成器（WAYLAND_DISPLAY 没设？）：{e}"))?;
        let mut queue = conn.new_event_queue::<Backend>();
        let qh = queue.handle();
        let mut backend = Backend::default();
        backend.registry = Some(conn.display().get_registry(&qh, ()));
        // 第一轮：绑定 shm / screencopy / xdg-output / wl_output，并收齐参数与 mode。
        queue.roundtrip(&mut backend)?;
        // 第二轮：为每个输出建 zxdg_output（拿逻辑位置与逻辑尺寸），再收名称。
        backend.attach_xdg_outputs(&qh);
        // 第三轮：seat 存在时建指针 / 键盘 / 剪贴板 / 光标形状。
        backend.wire_input(&qh);
        queue.roundtrip(&mut backend)?;
        Ok(Self { backend, queue, qh })
    }

    /// 阻塞等至少一个事件（阶段 1 的抓帧等待用）。
    pub fn blocking_dispatch(&mut self) -> Result<usize, Error> {
        Ok(self.queue.blocking_dispatch(&mut self.backend)?)
    }

    pub fn flush(&mut self) -> Result<(), Error> {
        self.queue.flush()?;
        Ok(())
    }
}

impl Backend {
    /// 覆盖层需要的全局对象是否齐备（缺失时给出可读的报错）。
    pub fn require_overlay_globals(&self) -> Result<(), Error> {
        if self.compositor.is_none() {
            return Err("合成器没有 wl_compositor".into());
        }
        if self.layer_shell.is_none() {
            return Err("合成器不支持 zwlr_layer_shell_v1（GNOME Wayland 不支持，见 ADR 0034）".into());
        }
        if self.seat.is_none() {
            return Err("合成器没有 wl_seat（无输入设备？）".into());
        }
        Ok(())
    }

    /// 有 seat 就建指针 / 键盘 / 剪贴板设备 / 光标形状设备。
    fn wire_input(&mut self, qh: &QueueHandle<Self>) {
        let Some(seat) = self.seat.clone() else {
            return;
        };
        self.pointer = Some(seat.get_pointer(qh, ()));
        self.keyboard = Some(seat.get_keyboard(qh, ()));
        if let Some(mgr) = self.data_device_manager.clone() {
            self.data_device = Some(mgr.get_data_device(&seat, qh, ()));
        }
        if let (Some(mgr), Some(pointer)) = (self.cursor_shape_manager.clone(), self.pointer.as_ref()) {
            self.cursor_shape_device = Some(mgr.get_pointer(pointer, qh, ()));
        }
        if let Some(mgr) = self.text_input_manager.clone() {
            self.text_input = Some(mgr.get_text_input(&seat, qh, ()));
        }
    }

    /// 有 `zxdg_output_manager_v1` 就给每个输出挂一个 `zxdg_output_v1`。
    /// 没有该协议（老合成器）则退回 `wl_output.geometry` 的位置信息。
    fn attach_xdg_outputs(&mut self, qh: &QueueHandle<Self>) {
        let Some(mgr) = self.xdg_output_manager.clone() else {
            return;
        };
        for (idx, info) in self.outputs.iter_mut().enumerate() {
            if let Some(out) = &info.output {
                info.xdg = Some(mgr.get_xdg_output(out, qh, idx));
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(m) = self.screencopy.take() {
            m.destroy();
        }
        if let Some(m) = self.xdg_output_manager.take() {
            m.destroy();
        }
    }
}

impl Dispatch<WlRegistry, ()> for Backend {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wayland_client::protocol::wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
            "zwlr_layer_shell_v1" => state.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
            "wl_data_device_manager" => state.data_device_manager = Some(registry.bind(name, version.min(3), qh, ())),
            "zwp_text_input_manager_v3" => state.text_input_manager = Some(registry.bind(name, version.min(1), qh, ())),
            "wp_cursor_shape_manager_v1" => {
                state.cursor_shape_manager = Some(registry.bind(name, version.min(1), qh, ()))
            }
            "zwlr_screencopy_manager_v1" => state.screencopy = Some(registry.bind(name, version.min(3), qh, ())),
            "zxdg_output_manager_v1" => state.xdg_output_manager = Some(registry.bind(name, version.min(3), qh, ())),
            "wl_output" => {
                let idx = state.outputs.len();
                state.outputs.push(Info::default());
                let out: wayland_client::protocol::wl_output::WlOutput = registry.bind(name, version.min(4), qh, idx);
                state.outputs[idx].output = Some(out);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: wayland_client::protocol::wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: wayland_client::protocol::wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgOutputManagerV1,
        _event: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
