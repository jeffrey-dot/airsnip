// SPDX-License-Identifier: GPL-3.0-only
//! 贴图（PIN-1…PIN-7）：把一块截图钉在屏幕最上层当便签。
//!
//! ## 坐标与表面模型（ADR 0034 §15）
//!
//! 贴图位置用**虚拟桌面物理像素**（与选区、覆盖层同一套坐标）。一个 Wayland surface
//! 只能属于一个输出，所以**每个输出各建一块 overlay 层表面**，各自：
//! - 覆盖整个输出（四边锚定 + size 0 → `wl_pointer` 局部坐标 + 该输出原点 = 桌面坐标）；
//! - 只画「贴图矩形 ∩ 本输出」的那一块；
//! - `set_input_region` 只留这块交集，其余区域点击穿透。
//!
//! 这样拖动可以跨屏（贴着鼠标在桌面坐标里走），也不会再有"表面自己移动导致指针
//! 坐标基准漂移"的问题（早期版本用 `set_margin` 移动小表面，动快点就飞出屏幕）。
//!
//! 缩放 / 透明度在写 `wl_shm` 缓冲时用 CPU 完成，不依赖合成器特效。
//! 生命周期：一个贴图一个进程（贴图要活到用户关掉，截图会话是"按完就退"）。

pub mod image;
pub mod input;
pub mod paint;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer as LayerKind;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self as layer_surface, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

use crate::Error;
use crate::wayland::{Backend, Session};
use image::PinImage;

/// 边框默认开（Windows 版 `Ctrl+F3` 开关边框；Wayland 侧改用 `B` 键）。
const DEFAULT_BORDER: bool = true;

/// 一个输出上的贴图表面。
pub struct PinLayer {
    /// 该输出在桌面坐标里的左上角。
    pub origin: (i32, i32),
    /// 该输出的物理尺寸。
    pub size: (i32, i32),
    pub surface: WlSurface,
    pub layer: ZwlrLayerSurfaceV1,
    /// 合成器给的逻辑尺寸（= 本输出）。
    pub logical: Option<(u32, u32)>,
    /// 双缓冲：拖动时交替使用，尽量不丢帧。
    pub bufs: Vec<PinBuf>,
    next: usize,
}

/// 一块 shm 缓冲（尺寸 = 一个输出的整屏）。
pub struct PinBuf {
    pub buffer: WlBuffer,
    pub map: memmap2::MmapMut,
    pub released: Arc<AtomicBool>,
    pub w: i32,
    pub h: i32,
    /// **这块缓冲自己**上次画进去的区域（表面局部坐标）。
    /// 必须按缓冲记录：两个缓冲轮流用时，若共用一份记录，会擦到另一块去，
    /// 每块自己的旧像素留着 → 拖动时拖出一串蓝框残影。
    pub last: Option<(i32, i32, i32, i32)>,
    _file: std::fs::File,
}

impl PinBuf {
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.map[..]
    }
}

/// 一块贴图（跨屏时由多个 `PinLayer` 拼起来）。
pub struct Pin {
    pub image: PinImage,
    /// 图像左上角，**桌面坐标**（虚拟桌面物理像素）。
    pub pos: (i32, i32),
    pub zoom: f32,
    pub opacity: u8,
    pub border: bool,
    pub layers: Vec<PinLayer>,
    /// 拖动状态。
    pub drag: Option<input::Drag>,
    pub closed: bool,
}

impl Pin {
    /// 缩放后的图像尺寸。
    pub fn scaled_size(&self) -> (i32, i32) {
        let w = (self.image.w as f32 * self.zoom).round().max(1.0) as i32;
        let h = (self.image.h as f32 * self.zoom).round().max(1.0) as i32;
        (w, h)
    }

    /// 贴图矩形（桌面坐标）。
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        let (w, h) = self.scaled_size();
        (self.pos.0, self.pos.1, w, h)
    }

    /// 为每个输出建一块覆盖整屏的 overlay 表面。
    pub fn create(
        backend: &Backend,
        qh: &QueueHandle<Backend>,
        image: PinImage,
        pos: (i32, i32),
    ) -> Result<Self, Error> {
        let compositor = backend.compositor.clone().ok_or("合成器没有 wl_compositor")?;
        let shell = backend
            .layer_shell
            .clone()
            .ok_or("合成器不支持 zwlr_layer_shell_v1（GNOME Wayland 不支持，见 ADR 0034）")?;
        let mut layers = Vec::new();
        for (i, info) in backend.outputs.iter().enumerate() {
            let Some(out) = info.output.clone() else { continue };
            let surface = compositor.create_surface(qh, input::PinSurfaceKey(i));
            let layer = shell.get_layer_surface(
                &surface,
                Some(&out),
                LayerKind::Overlay,
                "airsnip-pin".to_string(),
                qh,
                input::PinLayerKey(i),
            );
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_size(0, 0);
            layer.set_exclusive_zone(-1);
            // OnDemand：点一下才拿键盘（多个输出的表面都可以，合成器给被点的那个）
            layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            surface.commit();
            let (w, h) = info.buffer_size();
            layers.push(PinLayer {
                origin: info.canvas_pos(),
                size: (w, h),
                surface,
                layer,
                logical: None,
                bufs: Vec::new(),
                next: 0,
            });
        }
        if layers.is_empty() {
            return Err("合成器没有报任何输出".into());
        }
        Ok(Self {
            image,
            pos,
            zoom: 1.0,
            opacity: 255,
            border: DEFAULT_BORDER,
            layers,
            drag: None,
            closed: false,
        })
    }

    /// 到某个输出上的表面（按输出下标）。
    pub fn layer_mut(&mut self, idx: usize) -> Option<&mut PinLayer> {
        self.layers.get_mut(idx)
    }

    /// 整个虚拟桌面在桌面坐标里的包围盒（拖动钳制用）。
    pub fn desktop_rect(&self) -> (i32, i32, i32, i32) {
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for l in &self.layers {
            x0 = x0.min(l.origin.0);
            y0 = y0.min(l.origin.1);
            x1 = x1.max(l.origin.0 + l.size.0);
            y1 = y1.max(l.origin.1 + l.size.1);
        }
        (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
    }

    /// 准备某个输出的双缓冲（configure 之后调用）。
    pub fn ensure_buf(&mut self, idx: usize, shm: &wayland_client::protocol::wl_shm::WlShm, qh: &QueueHandle<Backend>) {
        let Some(layer) = self.layers.get_mut(idx) else { return };
        if !layer.bufs.is_empty() {
            return;
        }
        let (w, h) = layer.logical.unwrap_or((1, 1));
        let (w, h) = ((w as i32).max(1), (h as i32).max(1));
        for _ in 0..2 {
            if let Ok(buf) = create_buf(shm, qh, w, h) {
                layer.bufs.push(buf);
            }
        }
    }

    /// 取一块空闲缓冲（轮转）。
    pub fn take_buf(&mut self, li: usize) -> Option<usize> {
        let layer = self.layers.get_mut(li)?;
        for i in 0..layer.bufs.len() {
            let k = (layer.next + i) % layer.bufs.len();
            if layer.bufs[k].released.load(std::sync::atomic::Ordering::Acquire) {
                layer.next = (k + 1) % layer.bufs.len();
                return Some(k);
            }
        }
        None
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        for l in &self.layers {
            for b in &l.bufs {
                b.buffer.destroy();
            }
            l.layer.destroy();
            l.surface.destroy();
        }
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
        last: None,
        _file: file,
    })
}

impl Dispatch<ZwlrLayerSurfaceV1, input::PinLayerKey> for Backend {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrLayerSurfaceV1,
        event: layer_surface::Event,
        key: &input::PinLayerKey,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let shm = state.shm.clone();
        {
            let Some(pin) = state.pins.first_mut() else {
                return;
            };
            let Some(layer) = pin.layer_mut(key.0) else {
                return;
            };
            match event {
                layer_surface::Event::Configure { serial, width, height } => {
                    layer.layer.ack_configure(serial);
                    layer.logical = Some((width, height));
                }
                layer_surface::Event::Closed => {}
                _ => {}
            }
        }
        if let Some(shm) = shm {
            if let Some(pin) = state.pins.first_mut() {
                pin.ensure_buf(key.0, &shm, qh);
            }
        }
        // 区域先就位（默认区域是整块表面，会把下层窗口的点击全挡住）
        paint::update_input(state, 0);
        paint::paint_all(state);
    }
}

impl Dispatch<WlSurface, input::PinSurfaceKey> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as Proxy>::Event,
        _data: &input::PinSurfaceKey,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &wayland_client::protocol::wl_region::WlRegion,
        _event: <wayland_client::protocol::wl_region::WlRegion as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// 跑一次贴图：建表面、等 configure、进入事件循环直到关闭。
pub fn run(png_path: &std::path::Path, pos: (i32, i32)) -> Result<(), Error> {
    let img = image::load_png(png_path)?;
    // 由截图进程写出的临时文件，读完即可删
    if png_path.starts_with(std::env::temp_dir()) {
        std::fs::remove_file(png_path).ok();
    }
    let mut session = Session::connect()?;
    session.backend.require_overlay_globals()?;
    let qh = session.qh.clone();
    if let Some(seat) = session.backend.seat.clone() {
        session.backend.pin_pointer = Some(seat.get_pointer(&qh, input::PinPtr));
        session.backend.pin_keyboard = Some(seat.get_keyboard(&qh, input::PinKbd));
    }
    crate::wayland::debug(format!("贴图启动：位置 {pos:?}"));
    let pin = Pin::create(&session.backend, &qh, img, pos)?;
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
