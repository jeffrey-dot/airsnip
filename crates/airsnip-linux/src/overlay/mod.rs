// SPDX-License-Identifier: GPL-3.0-only
//! 截图覆盖层（CAP-2/3、OUT-1，标注见 EDT-*）：冻结画面 → 框选 → 画标注 →
//! 复制 / 另存 / 取消。
//!
//! 与 Windows 版的差别（ADR 0034）：一个 Wayland surface 只属于一个输出，
//! 所以是**每个输出一块 layer surface**，各自全屏、各自贴齐；选区与标注仍是
//! 画布级的单一坐标系，可以跨屏。

pub mod clipboard;
pub mod edit;
pub mod input;
pub mod objedit;
pub mod paint;
pub mod paths;
pub mod state;
pub mod surface;
pub mod text_input;
pub mod toolbar;

use crate::Error;
use crate::capture;
use crate::codec;
use crate::wayland::Session;
use state::{Action, Overlay};
use surface::Geom;

/// 跑一次截图会话。
pub fn run() -> Result<(), Error> {
    let mut session = Session::connect()?;
    session.backend.require_overlay_globals()?;

    // 1. 冻结画面（后续选区、导出都以它为准）
    let (canvas, shots) = capture::capture_all(&mut session)?;

    // 2. 每输出一块覆盖层
    let mut overlay = Overlay::new(&canvas).ok_or("分配会话画布失败")?;
    let qh = session.qh.clone();
    for (i, shot) in shots.iter().enumerate() {
        let scale = session.backend.outputs.get(i).map(|o| o.scale()).unwrap_or(1);
        let geom = Geom {
            phys: (shot.x, shot.y),
            size: (shot.w as i32, shot.h as i32),
            scale,
        };
        overlay
            .layers
            .push(surface::Layer::create(&session.backend, &qh, i, geom)?);
    }
    session.backend.overlay = Some(overlay);
    session.flush()?;

    // 3. 事件循环：直到某个键/动作把会话标记为完成
    loop {
        session.blocking_dispatch()?;
        if session.backend.overlay.as_ref().map(|o| o.done).unwrap_or(true) {
            break;
        }
    }

    // 4. 收尾：先算输出图（此时还能借用会话状态），再拆覆盖层
    let overlay = session.backend.overlay.take().ok_or("会话状态丢失")?;
    let action = overlay.action.unwrap_or(Action::Cancel);
    let serial = overlay.serial;
    let sel = overlay.current_sel();
    crate::wayland::debug(format!(
        "导出：选区 {sel:?}，对象 {} 个（选中 {:?}）",
        overlay.doc.objects().len(),
        overlay.selected
    ));
    let out = match (action, sel) {
        (Action::Cancel, _) => None,
        (_, None) => {
            eprintln!("airsnip：没有有效选区，忽略");
            None
        }
        (_, Some(sel)) => Some(paint::export(&overlay, sel)?),
    };

    // 复制：优先交给 wl-copy（它 fork 常驻，我们立刻退出），
    // 没有 wl-copy 才退回「自己持有选区并驻留」（此时 serial 必须还在有效期内，
    // 所以要在拆覆盖层之前设好）。
    let mut clip = None;
    let mut png_done = false;
    if let (Action::Copy, Some(canvas)) = (action, &out) {
        let png = codec::png_bytes(canvas)?;
        match clipboard::hand_off(&png) {
            Ok(()) => png_done = true,
            Err(e) => {
                crate::wayland::debug(format!("{e}；退回内置驻留模式"));
                clip = Some(clipboard::set(&mut session, png, serial)?);
            }
        }
    }
    let _ = png_done;

    drop(overlay); // 销毁 layer surfaces
    session.flush()?;

    match (action, out) {
        (Action::Pin, Some(canvas)) => {
            // 贴图要活到用户关掉 → 起一个常驻子进程（截图进程自己照常退出）
            let png = codec::png_bytes(&canvas)?;
            let path = std::env::temp_dir().join(format!("airsnip-pin-{}.png", std::process::id()));
            std::fs::write(&path, png)?;
            let exe = std::env::current_exe()?;
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("pin").arg(&path);
            // 贴图直接出现在选区原来的位置（桌面坐标）
            if let Some(s) = sel {
                cmd.arg("--pos").arg(format!("{},{}", s.x, s.y));
            }
            cmd.spawn()?;
            eprintln!("airsnip：已贴图（拖动摇位置 / 滚轮缩放 / Ctrl+滚轮透明度 / Esc 或双击关闭）");
        }
        (Action::Save, Some(canvas)) => {
            let path = paths::next_path();
            codec::write_png(&canvas, &path)?;
            eprintln!("airsnip：已保存 {}", path.display());
        }
        _ => {}
    }
    if let Some(clip) = clip {
        clipboard::serve(&mut session, clip)?;
    }
    Ok(())
}
