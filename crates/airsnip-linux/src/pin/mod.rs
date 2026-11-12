// SPDX-License-Identifier: GPL-3.0-only
//! 贴图（PIN-1…PIN-7）：把一块截图钉在屏幕最上层当便签。
//!
//! Wayland 上的实现选型（ADR 0034 §15）：
//! - 用 `zwlr_layer_shell_v1` 的 **overlay 层**：天然置顶、无边框、不进任务栏，
//!   与"贴图窗口"的语义一一对应；
//! - 位置用 **`anchor = Top|Left` + `margin(top, left)`** 精确摆放（wlroots 系通用做法），
//!   拖动时只改 margin，不重建表面；
//! - 缩放 / 透明度在**画进 shm 缓冲时**做（最近邻缩放 + alpha 乘系数），
//!   不依赖合成器特效，纯 CPU 且即时生效；
//! - 一个贴图一个进程：贴图要活到用户关掉，而截图会话是"按完就退"，
//!   两种生命周期分开最省事（每个贴图进程只持有自己那张图，约 10 MB）。

pub mod image;
pub mod input;
pub mod paint;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer as LayerKind;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self as layer_surface, Anchor, ZwlrLayerSurfaceV1,
};

use crate::Error;
use crate::wayland::{Backend, Session};
use image::PinImage;

/// 边框默认开（Windows 版 `Ctrl+F3` 开关边框；Wayland 侧改用 `B` 键）。
const DEFAULT_BORDER: bool = true;

/// 一块贴图。
pub struct Pin {
    pub surface: WlSurface,
    pub layer: ZwlrLayerSurfaceV1,
    /// 原图（RGBA8888）。
    pub image: PinImage,
    /// 左上角位置（物理像素，合成器坐标系）。
    pub pos: (i32, i32),
    /// 缩放倍数。
    pub zoom: f32,
    /// 透明度 0..=255。
    pub opacity: u8,
    pub border: bool,
    /// 合成器要求的逻辑尺寸（configure）。
    pub logical: Option<(u32, u32)>,
    pub bufs: Vec<PinBuf>,
    next: usize,
    /// 拖动状态（指针抓住表面时记录抓取点）。
    pub drag: Option<input::Drag>,
    /// 是否已要求退出（所有贴图关闭）。
    pub closed: bool,
}

/// 贴图用的一块 shm 缓冲。
pub struct PinBuf {
    pub buffer: WlBuffer,
    pub map: memmap2::MmapMut,
    pub released: Arc<AtomicBool>,
    pub w: i32,
    pub h: i32,
    _file: std::fs::File,
}

impl PinBuf {
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.map[..]
    }
}

impl Pin {
    /// 当前应显示的像素尺寸（缩放后）。
    pub fn scaled_size(&self) -> (i32, i32) {
        let w = (self.image.w as f32 * self.zoom).round().max(1.0) as i32;
        let h = (self.image.h as f32 * self.zoom).round().max(1.0) as i32;
        (w, h)
    }

    /// 建表面并摆到 `pos`。
    pub fn create(
        backend: &Backend,
        qh: &QueueHandle<Backend>,
        image: PinImage,
        output: Option<usize>,
        pos: (i32, i32),
    ) -> Result<Self, Error> {
        let compositor = backend.compositor.clone().ok_or("合成器没有 wl_compositor")?;
        let shell = backend
            .layer_shell
            .clone()
            .ok_or("合成器不支持 zwlr_layer_shell_v1（GNOME Wayland 不支持，见 ADR 0034）")?;
        let surface = compositor.create_surface(qh, input::PinSurface);
        let out = output
            .and_then(|i| backend.outputs.get(i))
            .and_then(|o| o.output.clone());
        let layer = shell.get_layer_surface(
            &surface,
            out.as_ref(),
            LayerKind::Overlay,
            "airsnip-pin".to_string(),
            qh,
            input::PinLayer,
        );
        layer.set_anchor(Anchor::Top | Anchor::Left);
        layer.set_margin(pos.1.max(0), 0, 0, pos.0.max(0));
        layer.set_exclusive_zone(-1);
        // OnDemand：点一下才拿键盘，避免贴图长期抢走所有按键
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        // 尺寸必须在**首次 commit 之前**设好：只锚定 Top|Left 时 width=0 是协议错误
        let (w0, h0) = (
            (image.w as f32).round().max(1.0) as u32,
            (image.h as f32).round().max(1.0) as u32,
        );
        layer.set_size(w0, h0);
        surface.commit();

        let pin = Self {
            surface,
            layer,
            image,
            pos,
            zoom: 1.0,
            opacity: 255,
            border: DEFAULT_BORDER,
            logical: None,
            bufs: Vec::new(),
            next: 0,
            drag: None,
            closed: false,
        };
        Ok(pin)
    }

    /// 位置变化后同步 margin（拖动 / 方向键）。
    pub fn apply_pos(&self) {
        self.layer.set_margin(self.pos.1.max(0), 0, 0, self.pos.0.max(0));
        self.surface.commit();
    }

    /// 缩放变化后同步表面尺寸。
    pub fn apply_zoom(&self) {
        let (w, h) = self.scaled_size();
        self.layer.set_size(w as u32, h as u32);
        self.surface.commit();
    }

    /// 准备双缓冲（configure 之后调用）。
    pub fn ensure_buffers(&mut self, shm: &wayland_client::protocol::wl_shm::WlShm, qh: &QueueHandle<Backend>) {
        if !self.bufs.is_empty() {
            return;
        }
        let (w, h) = self.scaled_size();
        for _ in 0..2 {
            if let Ok(buf) = create_buf(shm, qh, w, h) {
                self.bufs.push(buf);
            }
        }
    }

    /// 尺寸变了下重建缓冲（缩放时用）。
    pub fn reset_buffers(&mut self) {
        self.bufs.clear();
    }

    pub fn take_buffer(&mut self) -> Option<usize> {
        for i in 0..self.bufs.len() {
            let idx = (self.next + i) % self.bufs.len();
            if self.bufs[idx].released.load(Ordering::Acquire) {
                self.next = (idx + 1) % self.bufs.len();
                self.bufs[idx].released.store(false, Ordering::Release);
                return Some(idx);
            }
        }
        None
    }

    pub fn present(&self, buf: &PinBuf) {
        self.surface.attach(Some(&buf.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, buf.w, buf.h);
        self.surface.commit();
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        for b in &self.bufs {
            b.buffer.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
    }
}

fn create_buf(
    shm: &wayland_client::protocol::wl_shm::WlShm,
    qh: &QueueHandle<Backend>,
    w: i32,
    h: i32,
) -> Result<PinBuf, std::io::Error> {
    use std::os::fd::AsFd;
    let stride = w * 4;
    let len = stride as usize * h as usize;
    let path = format!(
        "/dev/shm/airsnip-pin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path).ok();
    file.set_len(len as u64)?;
    // SAFETY：映射长度取自刚 set_len 的文件尺寸，文件句柄由 `_file` 持有。
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    let released = Arc::new(AtomicBool::new(true));
    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        w,
        h,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        qh,
        released.clone(),
    );
    pool.destroy();
    Ok(PinBuf {
        buffer,
        map,
        released,
        w,
        h,
        _file: file,
    })
}

impl Dispatch<ZwlrLayerSurfaceV1, input::PinLayer> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrLayerSurfaceV1,
        event: layer_surface::Event,
        _idx: &input::PinLayer,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(pin) = state.pins.first_mut() else {
            return;
        };
        match event {
            layer_surface::Event::Configure { serial, width, height } => {
                pin.layer.ack_configure(serial);
                pin.logical = Some((width, height));
                if let Some(shm) = state.shm.clone() {
                    pin.ensure_buffers(&shm, qh);
                }
            }
            layer_surface::Event::Closed => pin.closed = true,
            _ => {}
        }
        paint::paint_all(state);
    }
}

impl Dispatch<WlSurface, input::PinSurface> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as Proxy>::Event,
        _data: &input::PinSurface,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// 跑一次贴图：建表面、等 configure、进入事件循环直到关闭。
pub fn run(png_path: &std::path::Path, output_name: Option<&str>) -> Result<(), Error> {
    let img = image::load_png(png_path)?;
    // 由截图进程写出的临时文件，读完即可删
    if png_path.starts_with(std::env::temp_dir()) {
        std::fs::remove_file(png_path).ok();
    }
    let mut session = Session::connect()?;
    session.backend.require_overlay_globals()?;
    let qh = session.qh.clone();
    // 贴图自己的 pointer / keyboard（udata 标记与覆盖层区分）
    if let Some(seat) = session.backend.seat.clone() {
        session.backend.pin_pointer = Some(seat.get_pointer(&qh, input::PinPtr));
        session.backend.pin_keyboard = Some(seat.get_keyboard(&qh, input::PinKbd));
    }
    // 初始位置：光标附近（拿不到光标位置时贴左上角偏移）
    let pos = (120, 120);
    // 指定输出：layer surface 不带 output 时由合成器「就近挑一个」，
    // 多屏下会落到别的屏上（实测 niri 就挑错了），所以由调用方把目标输出名传进来。
    let target = output_name.and_then(|name| {
        session
            .backend
            .outputs
            .iter()
            .position(|o| o.name.as_deref() == Some(name))
    });
    let pin = Pin::create(&session.backend, &qh, img, target, pos)?;
    session.backend.pins.push(pin);
    session.flush()?;

    loop {
        session.blocking_dispatch()?;
        let pins = &session.backend.pins;
        if pins.is_empty() || pins.iter().all(|p| p.closed) {
            break;
        }
    }
    session.backend.pins.clear();
    session.flush()?;
    Ok(())
}
