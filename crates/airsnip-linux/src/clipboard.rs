// SPDX-License-Identifier: GPL-3.0-only
//! 剪贴板：把 PNG 交给 `wl-copy`（它会 fork 一个常驻进程持有选区）。
//!
//! 截图覆盖层与贴图都用这一条路径；覆盖层另有"内置驻留"兜底（`overlay::clipboard`），
//! 因为那里需要在 `wl-copy` 缺失时仍能服务数据。

use std::io::Write;
use std::process::{Command, Stdio};

use crate::Error;

/// 把 PNG 交给 `wl-copy`：它 fork 常驻进程持有选区，本进程可以立刻退出。
pub fn hand_off(png: &[u8]) -> Result<(), Error> {
    let mut child = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 wl-copy 失败：{e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(png).map_err(|e| format!("写入 wl-copy 失败：{e}"))?;
    }
    let status = child.wait().map_err(|e| format!("等待 wl-copy 失败：{e}"))?;
    if !status.success() {
        return Err(format!("wl-copy 退出码 {status}").into());
    }
    crate::wayland::debug(format!("剪贴板：已交给 wl-copy（{} 字节）", png.len()));
    Ok(())
}
