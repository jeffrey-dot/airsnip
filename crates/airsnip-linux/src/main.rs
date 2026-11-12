// SPDX-License-Identifier: GPL-3.0-only
//! AirSnip 轻截 Linux 前端入口（ADR 0034）。
//!
//! 目标平台：**Wayland** 且支持 `wlr-screencopy` 的合成器（niri / sway / Hyprland /
//! KDE Plasma 6 等）。X11 与 GNOME Wayland 暂不支持，理由见 ADR 0034。
//!
//! 当前阶段只提供抓屏命令，用来验证协议链路：
//!
//! ```text
//! airsnip capture              # 抓全屏 → ./airsnip-capture.png
//! airsnip capture -o a.png     # 指定输出文件
//! airsnip capture --list       # 只列出输出信息（名称 / 位置 / 尺寸 / 缩放）
//! airsnip capture --output DP-1  # 只抓一个输出
//! ```

mod canvas;
mod capture;
mod codec;
mod draw;
mod overlay;
mod pin;
mod wayland;

use std::path::PathBuf;
use std::process::ExitCode;

/// 本 crate 的错误类型：阶段 1 用字符串化错误，避免为几行代码引 `anyhow`。
pub type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("airsnip: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), Error> {
    match args.first().map(String::as_str) {
        Some("capture") => cmd_capture(&args[1..]),
        Some("snip") => overlay::run(),
        Some("pin") => cmd_pin(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("未知子命令：{other}（用 --help 看用法）").into()),
    }
}

fn print_help() {
    println!(
        "AirSnip（Linux 前端，版本 {}）\n\n\
         用法：\n  \
           airsnip snip                 # 截图：覆盖层 → 框选 → Enter 复制 / Ctrl+S 保存\n  \
  airsnip pin <图片>           # 把一张图片钉在屏幕最上层（拖动/缩放/透明度/关闭）\n  \
  airsnip capture [-o 文件] [--output 名称] [--list]\n\n\
         说明：\n  \
           snip      冻结画面并弹出覆盖层，框选后 Enter 复制、Ctrl+S 另存、Esc 取消\n  \
           capture   抓取当前所有输出的画面（走 wlr-screencopy），合成一张 PNG\n  \
           --list    只打印输出信息，不抓屏\n  \
           --output  只抓指定输出（名称用 --list 查）\n\n\
         环境要求：Wayland 合成器需支持 zwlr_screencopy_manager_v1\n\
         （niri / sway / Hyprland / KDE Plasma 6 均支持）。",
        env!("CARGO_PKG_VERSION")
    );
}

fn cmd_pin(args: &[String]) -> Result<(), Error> {
    let mut path: Option<PathBuf> = None;
    let mut pos = (120, 120);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pos" => {
                i += 1;
                let v = args.get(i).ok_or("--pos 需要 x,y")?;
                let (x, y) = v.split_once(',').ok_or("--pos 格式应为 x,y")?;
                pos = (x.trim().parse()?, y.trim().parse()?);
            }
            other => path = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let path = path.ok_or("用法：airsnip pin <图片路径> [--pos x,y]")?;
    pin::run(&path, pos)
}

fn cmd_capture(args: &[String]) -> Result<(), Error> {
    let mut out: Option<PathBuf> = None;
    let mut only: Option<String> = None;
    let mut list = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" => {
                i += 1;
                out = Some(PathBuf::from(args.get(i).ok_or("-o 需要一个文件路径")?));
            }
            "--output" => {
                i += 1;
                only = Some(args.get(i).ok_or("--output 需要一个输出名")?.clone());
            }
            "--list" => list = true,
            other => return Err(format!("未知参数：{other}").into()),
        }
        i += 1;
    }

    let mut session = wayland::Session::connect()?;
    if session.backend.screencopy.is_none() && !list {
        eprintln!("提示：合成器未提供 zwlr_screencopy_manager_v1，无法抓屏。");
    }

    if list {
        for (idx, info) in session.backend.outputs.iter().enumerate() {
            let (w, h) = info.buffer_size();
            println!(
                "[{idx}] {name}  位置={pos:?}  缓冲={w}×{h}  缩放={scale}  transform={tf}",
                name = info.label(),
                pos = info.canvas_pos(),
                scale = info.scale(),
                tf = info.transform,
            );
        }
        return Ok(());
    }

    let (mut canvas, shots) = capture::capture_all(&mut session)?;
    if let Some(name) = &only {
        let Some(shot) = shots.iter().find(|s| &s.name == name) else {
            let names: Vec<&str> = shots.iter().map(|s| s.name.as_str()).collect();
            return Err(format!("没有叫 {name} 的输出，可选：{}", names.join(", ")).into());
        };
        // 单输出模式：从合成画布按该输出位置裁出来
        let ox = shots.iter().map(|s| s.x).min().unwrap_or(0);
        let oy = shots.iter().map(|s| s.y).min().unwrap_or(0);
        canvas = canvas.crop(shot.x - ox, shot.y - oy, shot.w, shot.h);
    }

    let path = out.unwrap_or_else(|| PathBuf::from("airsnip-capture.png"));
    codec::write_png(&canvas, &path)?;
    println!(
        "已保存 {}（{}×{}，{} 个输出）",
        path.display(),
        canvas.w,
        canvas.h,
        shots.len()
    );
    Ok(())
}
