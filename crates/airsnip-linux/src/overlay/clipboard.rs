// SPDX-License-Identifier: GPL-3.0-only
//! 剪贴板：把出图作为 `image/png` 放进 Wayland 选区。
//!
//! Wayland 的剪贴板是**数据源**模型——复制方必须活着等粘贴方来取数据，
//! 不是「写进去就走」。所以复制完成后本进程会继续等到数据被取走（或选区被别的
//! 客户端顶掉）才退出，行为等价于 `wl-copy`（且只服务一次粘贴，同 `--paste-once`）。
//! 不 shell out 到 `wl-copy`：那会让「绿色单文件」多一个运行时依赖。

use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};

use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_data_offer::WlDataOffer;
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::Error;
use crate::wayland::{Backend, Session};

/// 交给 `wl-copy` 持有选区（实现见 `crate::clipboard`，贴图也用同一个）。
pub use crate::clipboard::hand_off;

/// 数据源的共享状态。
#[derive(Default)]
pub struct ClipData {
    pub png: Vec<u8>,
    /// 已经被粘贴方取走一次。
    pub sent: bool,
    /// 选区被别的客户端顶掉（无需再等）。
    pub cancelled: bool,
}

/// 把 PNG 字节设为剪贴板选区（**必须在覆盖层销毁前调用**：Wayland 要求
/// `set_selection` 的 serial 来自「仍然有效」的输入事件，覆盖层一拆，
/// 键盘焦点就没了，多数合成器会直接忽略这个请求）。
///
/// 返回数据源句柄，调用方随后用 `serve` 等粘贴方来取。
pub fn set(session: &mut Session, png: Vec<u8>, serial: u32) -> Result<ClipHandle, Error> {
    let mgr = session
        .backend
        .data_device_manager
        .clone()
        .ok_or("合成器没有 wl_data_device_manager")?;
    let device = session
        .backend
        .data_device
        .clone()
        .ok_or("没有 wl_data_device（缺 wl_seat？）")?;
    if serial == 0 {
        return Err("没有可用的输入 serial，无法设置剪贴板".into());
    }

    let n = png.len();
    let data = Arc::new(Mutex::new(ClipData {
        png,
        sent: false,
        cancelled: false,
    }));
    let source = mgr.create_data_source(&session.qh, data.clone());
    source.offer("image/png".to_string());
    device.set_selection(Some(&source), serial);
    // 立刻把请求推给合成器：后面如果马上拆覆盖层，也不能丢这条消息。
    session.flush()?;
    crate::wayland::debug(format!("剪贴板：已 set_selection（serial={serial}，{n} 字节 PNG）"));
    Ok(ClipHandle { data, _source: source })
}

/// 数据源句柄：`serve` 期间必须活着，否则合成器会认为源已销毁。
pub struct ClipHandle {
    data: Arc<Mutex<ClipData>>,
    _source: WlDataSource,
}

/// 等粘贴方来取数据，**持续服务到选区被别的客户端顶掉为止**。
///
/// 这不是「取走一次就退出」能解决的：Wayland 没有服务端存储，选区只存在于数据源
/// 进程存活期间。实测第一次 `send` 的请求方可能是 XWayland 之类的桥接（拿完就丢），
/// 若此时我们退出，用户真正粘贴时选区已经空了。所以行为对齐 `wl-copy`：
/// 常驻服务、多次请求都响应，直到收到 `cancelled`（自己的选区被顶掉）。
pub fn serve(session: &mut Session, clip: ClipHandle) -> Result<(), Error> {
    loop {
        let (sent, cancelled) = {
            let d = clip.data.lock().map_err(|_| "剪贴板状态锁被污染")?;
            (d.sent, d.cancelled)
        };
        let _ = sent;
        if sent {
            // 已至少服务过一次；继续等（可能还会有别的请求方，见函数注释）。
            crate::wayland::debug("剪贴板：已服务过一次，继续驻留等待下一次请求");
        }
        if cancelled {
            crate::wayland::debug("剪贴板：选区被其它客户端顶掉");
            break;
        }
        // 注意：`blocking_dispatch` 返回 0 只代表这一轮没派发事件（可能被唤醒），
        // **不**代表连接结束；这里必须继续等，否则会在用户粘贴之前就把数据源销毁。
        session.blocking_dispatch()?;
    }
    Ok(())
}

impl Dispatch<WlDataDeviceManager, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDevice,
        _event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // data_offer / enter / leave / motion / drop / selection 都不需要：
        // AirSnip 只写剪贴板，不读。
    }

    // `data_offer` 事件会创建子对象；我们不对它做任何事，但仍要给出用户数据，
    // 否则 wayland-client 会直接 panic（默认实现就是 panic）。
    wayland_client::event_created_child!(Backend, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataOffer,
        _event: wayland_client::protocol::wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // 只写不读：忽略其它客户端放的 mime 列表。
    }
}

impl Dispatch<WlDataSource, Arc<Mutex<ClipData>>> for Backend {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        data: &Arc<Mutex<ClipData>>,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                crate::wayland::debug(format!("剪贴板：对方请求 {mime_type}"));
                if mime_type != "image/png" {
                    return;
                }
                let Ok(mut d) = data.lock() else {
                    return;
                };
                let n = d.png.len();
                let mut file = File::from(fd);
                match file.write_all(&d.png) {
                    Ok(()) => {
                        d.sent = true;
                        crate::wayland::debug(format!("剪贴板：已写入 {n} 字节"));
                    }
                    Err(e) => crate::wayland::debug(format!("剪贴板：写入失败 {e}")),
                }
            }
            wl_data_source::Event::Cancelled => {
                if let Ok(mut d) = data.lock() {
                    d.cancelled = true;
                }
            }
            _ => {}
        }
    }
}
