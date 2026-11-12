# ADR 0034：Linux（Wayland）前端与 workspace 拆分

- 状态：**进行中**（S1–S4 已交付并在实机装机验证；S4b / S5 待做，见 §6）
- 影响范围：`Cargo.toml`（改成 workspace root，根包仍是 Windows 前端）、
  `crates/airsnip-core/`（新增，`editor/` 迁入）、`crates/airsnip-linux/`（新增）、
  `src/main.rs`（`pub mod editor` → `pub use airsnip_core::editor`）
- 相关：README「系统要求」、PRD §6.2 模块划分、ADR 0021（静态 CRT）、ADR 0030（标注工具）、
  ADR 0033（工具栏可见性）

## 背景

AirSnip 是纯 Windows 程序：31,146 行 Rust、164 个文件，其中 **78 个文件直接 `use windows::`**
（app 86 处、overlay 47、settings 28、pin 27、longshot 26、capture 24、ocr 14、update 9、
render 9、output 8、theme 4）。README 的「系统要求」写明只出 `win-x64`、需要 Windows 10+。

需求方的运行环境是 **Arch Linux + niri**（Wayland，wlroots 系协议），要求原生运行，
明确拒绝虚拟机。所以这不是「加个 `cfg` 就能编」的程度，而是一次真实的移植。

已实测的环境能力（同一台机器，2026-11）：

| 能力 | 现状 | 结论 |
|---|---|---|
| 合成器 | niri 26.04，两块 1920×1080 输出（DP-1 在 x=1920，HDMI-A-1 在 x=0），缩放均为 1 | — |
| 抓屏 | `grim` 抓到 3840×1080 → 合成器支持 `zwlr_screencopy_manager_v1` | ✅ 走协议 |
| 全局热键 | niri `config.kdl` 的 `binds { spawn "…" }` 正常工作 | ✅ 由合成器触发 |
| 剪贴板图片 | `wl-copy` 可用 | ✅ |
| 中文字体 | 思源黑体 / Noto CJK 齐全 | ✅ |
| OCR | 装了 `tesseract`，但只有 `afr` / `osd` 语言包 | ⚠️ 需装 `tesseract-data-chi_sim` |
| 托盘 | niri 无托盘，StatusNotifier 还需要 bar 支持 | ⚠️ 改为热键触发、无托盘 |

## 决策

### 1. 仓库改成 workspace，根包仍是 Windows 前端

```
Cargo.toml            # [package] airsnip（Windows 前端）+ [workspace] members = crates/*
crates/airsnip-core/  # 平台无关：标注对象模型、文档撤销栈（纯 std）
crates/airsnip-linux/ # 新前端：Wayland（bin 名也叫 airsnip）
```

**为什么不是「新建 crate 放 Linux，Windows 侧原封不动」之外的第三种做法**：根包保持 `airsnip`
不动，意味着 `cargo build --release`、`cargo test`、`scripts/package.ps1`（找
`target/release/airsnip.exe`）、`build.rs` 的相对路径（`assets/`、`app.rc`）、
`include_bytes!("../../assets/…")` 全部照旧 —— 迁移风险集中在一个新增 crate 里。

代价与边界：

- `cargo build --workspace` **不能**用（两个 bin 都叫 `airsnip`，会撞 `target/debug/airsnip`；
  且 Windows 前端在 Linux 上本来就编不过）。Linux 侧一律 `cargo build -p airsnip-linux`。
- `cargo test` 在仓库根只跑 Windows 侧 333 项；`editor` 的 **46 项**已随代码迁到
  `cargo test -p airsnip-core`。CI/本地检查两边都要跑（见 AGENTS.md「常用命令」）。

### 2. 先把 `editor/` 抽成 core，且**零业务改动**

`editor/`（2,061 行）本来就没有任何 `windows::` 依赖，也不依赖渲染层或其它模块
（只用 `editor::` 自身的名字），是天然的 core 候选。抽取方式：

- 代码用 `git mv` 原样搬走，只把 `pub(crate) mod palette` / `pub(crate) mod text`
  改成 `pub mod`（跨 crate 需要）；
- Windows 前端在 `src/main.rs` 写 `pub use airsnip_core::editor;`，全仓库既有的
  `crate::editor::…` 路径**一个都不用改**（`use` 绑定在 crate root 上就是
  `crate::editor`）。diff 里业务代码零改动，46 项测试原样通过。

core 的硬边界：**只放纯数据 + 纯算法**，不得引入平台 API，也不得依赖 tiny-skia / 任何渲染后端。
渲染永远是平台前端的事（Windows 用 DirectWrite+GDI，Linux 用 cosmic-text+wl_shm）。

### 3. 抓屏直接走 `wlr-screencopy`，不 shell out `grim`

协议链路（`crates/airsnip-linux/src/capture/`）：`wl_shm` 建一块
`Xrgb8888` 缓冲（`/dev/shm` 临时文件 + `mmap`，建完立刻 unlink）→
`zwlr_screencopy_manager_v1.capture_output` → 等 `buffer` / `flags` / `ready` →
把像素按**输出逻辑位置**写进一张覆盖整个输出的画布。

- **位置来源优先 `zxdg_output_v1`**（`logical_position` / `logical_size` / `name`），
  退回 `wl_output.geometry`：部分合成器 geometry 恒为 0，只靠它会拼错。
- **缓冲区尺寸 = 当前 mode 尺寸，随 `transform` 交换长短边**；`flags.y_invert`
  也要处理（GL 合成器可能上下颠倒）。
- 不做「每屏先存一张临时位图再拼」：位置在抓屏前就已知，所以先算包围盒、
  分配画布，再逐个输出**直接写进画布**——全屏只多一次像素拷贝。

实测（release，两块 1080p，共 3840×1080）：**抓屏+合成 39 ms**，内存临时占用一块画布
（3840×1080×4 ≈ 16.6 MB）。与 `grim` 同刻对比，**逐像素差异 0.05%**（只有时钟等实时内容），
通道均值完全一致（证明 `Xrgb8888` 的 BGRX→RGBA 顺序正确）。

### 4. 目标平台只承诺 wlroots 系协议族

`wlr-screencopy` + `wlr-layer-shell` 是 wlroots 系（niri / sway / Hyprland / KDE Plasma 6）
的通行做法。**明确不做**：

- **GNOME Wayland**：不支持这两个协议，抓屏只能走 `xdg-desktop-portal` + PipeWire 流，
  覆盖层也没法自由定位 —— 属于另一条实现路径，将来单独开 ADR。
- **X11**：需求方环境是 Wayland；X11 版会让「覆盖层/输入/热键」三套实现并存，
  收益与维护成本不成比例。

版本过旧、缺协议的合成器直接报可读错误（`合成器不支持 zwlr_screencopy_manager_v1`），
不做静默降级（例如偷偷调 `grim`）——静默降级会让「为什么慢/为什么图不对」无从排查。

### 5. 热键与常驻形态：交给合成器，不做守护进程

Windows 版靠 `RegisterHotKey` + 托盘常驻。Wayland **没有**全局热键的公开协议
（`GlobalShortcuts` portal 在 niri 上不可用），因此：

- 热键由**合成器配置**触发：`binds { Mod+F1 { spawn "airsnip" "snip"; } }`；
- 不注册热键 → 不需要常驻 → 「待机 0% CPU / ≤15 MB」在 Linux 侧自动满足（进程根本不活着）；
- 托盘同理取消。代价：用户要改一次合成器配置（会在 README / 安装说明里给出 niri 与 sway 的片段）。

### 6. 分阶段交付（每阶段独立可验证、独立 PR）

| 阶段 | 内容 | 验收标准 | 状态 |
|---|---|---|---|
| **S1** | workspace 拆分 + `airsnip-core` + `wlr-screencopy` 抓屏落 PNG | `airsnip capture` 出的图与 `grim` 逐像素一致（差异只来自实时内容） | ✅ 本次 |
| **S2** | `wlr-layer-shell` 全屏覆盖层：冻结画面、框选、`Enter` 复制到剪贴板、`Ctrl+S` 另存 PNG、`Esc` 取消 | 按 niri 绑定 → 出覆盖层 → 框选 → 粘到别的程序 | ✅ 已交付 |
| **S3** | 绘制层上移到 `airsnip-core` + `TextSink` 抽象（DirectWrite / cosmic-text 双实现） | 两个平台共用一份对象绘制代码 | ✅ 已交付 |
| **S4** | 10 种标注工具 + **工具栏**（工具 / 5 色块 / 填充 / 撤销重做 / 复制 / 保存） | 与 Windows 版工具集对齐 | ✅ 已交付并装机（`~/.local/bin/airsnip`，Alt+A 绑定在 niri） |
| **S4b** | 文本工具 + 中文输入法（text-input-v3） | 中文能上屏、能提交为文本对象 | ✅ 已交付 |
| **S4c** | 对象选中 / 移动 / 缩放 / 删除 / 方向键微调 / 滚轮线宽 / 颜色·填充作用于选中对象 | 标注能改 | ✅ 已交付 |
| **S4d** | 选区尺寸提示、`Shift` 绘制约束、箭头曲率控制点、圆角手柄 | 与 Windows 版对齐 | ✅ 已交付 |
| **S5** | 贴图（layer-shell overlay 层）、长截图（复用拼接算法）、OCR（Tesseract，含语言包检测）、多格式输出 | 与 Windows 版功能对等 | ⬜ |

阶段顺序的理由：S1 把「协议链路」的不确定性一次性消掉（已证明可行）；
S2 是「能用」的最小闭环；S3 才动文本渲染这块最大的移植工作；S4/S5 是体量问题而非风险问题。

### 7. 绘制层上移（S3/S4 的前提）

原先 `src/render/{mod,region,bake}.rs` 里 900 多行「对象 → 像素」的绘制代码已经是
`windows::` 零引用，只差三个文本调用点。为了让两个前端**共用一份**绘制实现：

- `render/{mod,region,bake,stroke,blur,mask,icon}.rs` → `airsnip_core::draw::*`；
- 文本栅格化抽成 `draw::text::TextSink`（`measure` + `draw`）：Windows 侧 40 行适配器接
  DirectWrite，Linux 侧接 cosmic-text；
- Windows 的 `src/render/mod.rs` 变成**薄壳**（包装函数 + DirectWrite 适配器），
  调用点 `render::draw_objects(…)` 等一律不变；
- 验证手段：`cargo check --target x86_64-pc-windows-msvc --all-targets` 在 Linux 上通过
  （`check` 不需要 MSVC SDK 链接，`cargo-xwin` 下载失败也不影响）。**Windows 改动从此可在
  Linux 侧自查**，这是本次最大的工程收益。

S4 用到但尚未迁移的：`render/text.rs`（DirectWrite 本体）、`render/fonts.rs`（字体枚举）、
`overlay/toolbar/*`（工具栏布局与绘制，约 900 行，仅 `tip.rs` 有一处 `windows::`）。

### 8. 工具栏（S4）

`crates/airsnip-linux/src/overlay/toolbar.rs`：与 Windows 版同样的分组
（工具 10 ｜ 5 色块 ｜ 填充·撤销·重做 ｜ 复制·保存），单行、圆角深色底、
当前工具与选中色块高亮、按钮悬停底色。要点：

- **布局是纯函数**：`layout(canvas_w, canvas_h, sel) -> Toolbar`，绘制与命中测试用同一个
  结果，不存在「画在一处、点在另一处」的错位；每帧现算，无需缓存状态。
- 位置：选区下方优先，塞不下翻到上方，再钳进画布；工具栏比画面还宽时从左边距起画、
  右侧裁掉（与 ADR 0033 记录的已知边界一致，出路是隐藏元素）。
- 图标复用 `airsnip_core::draw::icon`（Material Symbols，与 Windows 同批 SVG）；
  圆角矩形复用 core 的 `rounded_rect_path`，没有第二份实现。
- 交互优先级：**按下先问工具栏**——命中就只处理按钮（并置 `toolbar_hold`，松手不结束
  框选/绘制），未命中才走「选区内画标注 / 选区外重新框选」。
- 视觉自检：`AIRSNIP_DEBUG=1 cargo test -p airsnip-linux toolbar_renders` 会落盘
  `/tmp/airsnip-toolbar.png`，直接看图即可（本次就是靠它确认图标与色块都画对）。

### 9. S2 落地要点（覆盖层）

- **每个输出一块 layer surface**（overlay 层、四边锚定、`exclusive_zone = -1`、键盘独占），
  一个会话可以有 N 块，选区是画布级的单一矩形，可跨屏。不用普通 toplevel：niri 是滚动平铺，
  toplevel 会被纳入布局逻辑，全屏与键盘独占都不可靠。
- **双缓冲 + 丢帧**：没拿到已释放的缓冲就跳过这一帧，不在合成器忙时堆请求。
- **首帧在 configure 里画**：否则用户会看到「黑一下」。实测两块屏都拿到 `configure 1920×1080`。
- **单缓冲只画一次暗化**：`dim` 在会话开始时算好，每帧只做 memcpy + 选区回贴 + 边框/手柄填充，
  不重复算像素（S2 因此不需要 tiny-skia）。
- **光标**：`wp_cursor_shape_manager_v1` 设 crosshair；协议缺失时保持默认光标（不报错）。
- **剪贴板**：Wayland 是数据源模型——复制方要活着等粘贴方来取。所以 `Enter` 之后进程会继续
  等到数据被取走或选区被别的客户端顶掉才退出（等价 `wl-copy`，只服务一次粘贴）。
  不做成 `wl-copy` 子进程：那会给「绿色单文件」加一个运行时依赖。
- **保存**：S2 固定写 `$XDG_PICTURES_DIR/airsnip-YYYYMMDD-HHMMSS.png` 并打印路径，
  不弹对话框（Wayland 要 `xdg-desktop-portal` 的 FileChooser，归到 S5）。
- **键位直接比 evdev 码**（Esc=1 / Enter=28 / C=46 / S=31 / Ctrl=29 或 97），不解析 xkb keymap。

### 10. 两个实测出来的坑（S2/S4 修）

**① 复制到剪贴板：`set_selection` 必须在覆盖层销毁之前**。Wayland 要求
`wl_data_device.set_selection` 的 serial 来自仍然有效的输入事件；先把 layer surface
拆掉（键盘焦点随之消失）再设选区，合成器会直接忽略这个请求 —— 表现为「按了 Enter，粘贴是空的」。

**② 剪贴板数据源必须**持续驻留**，不是「交一次就退」**。Wayland 没有服务端存储，
选区只存在于数据源进程活着的时候。实测第一次 `send` 的请求方往往是 XWayland 之类的桥接
（拿完不保存），此时若进程退出，用户真正粘贴时选区已经空了（`wl-paste --list-types`
直接报 `Nothing is copied`）。现在的行为对齐 `wl-copy`：常驻、多次请求都响应，
直到收到 `cancelled`（自己的选区被别的客户端顶掉）才退出。

**③ 剪贴板交接**：既然「数据源必须活着」是 Wayland 的硬约束，就别自己扛——把 PNG 交给
`wl-copy`（它会 fork 一个常驻进程持有选区），AirSnip 立刻退出。实测与本机 DMS quickCapture /
Flameshot 的行为一致：`printf x | wl-copy` 秒返回、数据仍在剪贴板；`pgrep airsnip` 为空，
持有者是 `wl-copy --type image/png`。`wl-copy` 不存在时退回内置驻留（`set` + `serve`）。

**④ 导出裁剪图全空白**：`copy_block` 的源矩形与目标偏移必须分开传。早先两者共用同一组
坐标（源在选区坐标、目标从 0 开始），裁剪图比源小，整块被钳掉 —— 导出的 PNG 全透明。
现有单测 `export_crops_the_selected_region` / `export_bakes_annotations_on_top` 守门。

### 11. 交付与验证记录（装机环境）

- 环境：Arch Linux + **niri 26.04**，双输出 1920×1080（DP-1 逻辑 x=1920、HDMI-A-1 x=0），缩放均为 1
- 抓屏：release 下 **抓屏+合成 39 ms**（3840×1080），与 `grim` 同刻对比逐像素差异 0.05%（仅实时内容）
- 交互：用 `ydotool`（uinput 合成鼠标/键盘）跑通「Alt+A → 覆盖层 → 框选 → 工具栏画标注 → `Enter`」，
  剪贴板取回为有效 PNG，且**复制后 AirSnip 进程退出**（选区交给 `wl-copy` 持有）
- 回归：`cargo test -p airsnip-core -p airsnip-linux` = 83 + 24 项；`cargo deny check` 四类全绿；
  Windows 侧 `cargo check --target x86_64-pc-windows-msvc --all-targets` 通过（Linux 上交叉 check）

### 12. 文本工具（S4b）与输入法

Wayland 没有 Windows 的 `EDIT` 子控件可用，所以文本编辑是自己做的一层最小状态机
（`overlay/text_input.rs`）：

- 点击放置 → `zwp_text_input_v3.enable()` 请求输入法 → 收 `commit_string` 追加 → 
  `Esc` / 点击别处结束 → 非空则成为一个普通 `Kind::Text` 对象（渲染与导出都走 core）
- 没有输入法时退回按键：evdev → US 布局可打印字符（`key_char`），覆盖常用字母数字符号
- 编辑态**不写历史**：提交成功才算一次可撤销编辑（与 Windows 版一致）

**输入法候选框定位**：`zwp_text_input_v3.set_cursor_rectangle` 用的坐标是**焦点表面的局部坐标**，
早期图省事传 `(0,0,0,0)`，候选框就钉在表面左上角。现在按「文字左上角 + 已输入宽度」算插入点，
再减去键盘焦点所在输出的原点后下发；文字增删 / 输入法上屏 / 焦点切换都会重新同步。

**实测踩到的坑（很重要）**：输入法会把 `Enter` 吃掉——合成器只回 `commit_string`，
我们的 `wl_keyboard.key` 根本收不到 Enter。于是：
1. 结束输入改用 `Esc` / 点击别处（`Enter` 只在**没有**输入法接管时作为提交键）；
2. 编辑态下 `Esc` 是**提交**而不是丢弃——否则输入被吞后用户没有"结束但不丢字"的键。
   这与 Windows 版（`Esc` 取消输入）不同，属于平台差异，已写进 README。

另外把默认色块从白改成**红**（与 Windows 版一致）：荧光笔是正片叠底，白色 = 像素不变，
用白色画高亮看起来"完全没反应"。

**聚光（Glow）的显示管线**：`Kind::Glow` 在对象绘制里是**空实现**（Windows 版也一样），
效果由"选区内整体暗化 + 把每块聚光区域恢复成原图亮度"提供；Linux 侧原先漏了这层，
现在 `paint::rebuild` 里补齐（圆角用 core 的 `mask::blend_compact` 插值），导出侧 `draw::bake` 本来就是对的。

### 13. 对象编辑（S4c）

`crates/airsnip-linux/src/overlay/objedit.rs`。几何全部复用 core（`Kind::hit` /
`translate` / `remap`），交互沿用 Windows 版 ADR 0023 的约定：

| 操作 | 行为 |
|---|---|
| 点中对象（包围框内） | 选中，显示虚线框 + 八向手柄 |
| `Ctrl` + 拖动对象 | 移动 |
| 拖手柄 | 缩放（箭头 / 直线只有两端点手柄，拖动改端点并带走控制点） |
| `Del` / `Backspace` | 删除，`Ctrl+Z` 一步恢复 |
| 方向键 / `Shift`+方向键 | 1px / 5px 微调 |
| 滚轮 | 有选中改选中对象的线宽，否则改画笔 |
| 色块 / 填充 | 有选中改选中对象，否则作用于下一次绘制 |

两个与"看起来该怎样"不同的取舍：

1. **未填充矩形只有描边能"严格命中"**（与 Windows 一致），但**选中/移动用包围框宽松命中**——
   否则用户点矩形内部会变成"又画一个矩形"。判定「画还是选」仍靠严格命中 + 有无位移。
2. **拖动必须每帧从按下时的几何重算**。最初实现在"已经移动过的对象"上再做一次 `remap`，
   位移逐帧累积：拖 10 帧跑出去约 10 倍距离（实测把矩形直接拖出裁剪范围，导出图里"消失"）。
   现在 `Drag` 里存按下时的 `Kind` 快照，`repeated_drag_frames_do_not_accumulate` 守门。

### 14. 尺寸提示 / Shift 约束 / 曲线控制点 / 圆角手柄（S4d）

- **尺寸提示**：`"宽 × 高"`，16px 粗体 + 1px 黑色阴影（纯白字在浅底上看不清）。
  位置取工具栏的**对面**一侧（工具栏在选区下方 → 文字在上方），上方放不下翻到下方，再钳进画布。
  只上屏不导出（`paint::draw_size_hint` 在 `rebuild` 里画）。
- **`Shift` 约束**：几何直接复用 core —— 矩形类走 `square_corner`（正方形），
  箭头 / 直线走 `snap_direction`（水平 / 垂直 / 45°）；约束后再钳回选区。
- **箭头曲率控制点**：`Kind::control_points()` 给出 1/3、2/3 两个点，画虚线连端点 + 白圆点；
  拖动改 `c1`/`c2`。命中优先于端点手柄（控制点常落在线身上）。
- **圆角手柄**：位置 `inset = 14 + 0.293 × r`（夹到短边 45%），朝中心拖增大、朝角拖减小
  （`radius_delta = (dy - dx) / 2`）；拖动时同时把圆弧描出来，半径变化看得见。
  `Kind::set_corner` 负责夹到 `[0, 短边一半]`。

**架构调整**：`overlay/objedit.rs` 拆成 `objedit/{mod,paint}.rs`（守住单文件 300 行红线），
`mod.rs` 只放几何与交互，`paint.rs` 只放装饰绘制。

### 15. 贴图（S5，PIN-1…PIN-7）

`crates/airsnip-linux/src/pin/`。选型与踩点：

- **表面**：`zwlr_layer_shell_v1` 的 **overlay 层**，**每个输出一块覆盖整屏的表面**，
  各自只画「贴图 ∩ 本输出」，并把输入区域限制在该交集内（其余区域点击穿透）。
  贴图位置用**虚拟桌面坐标**，因此可以跨屏拖动。

  *走过的弯路（两次用户实测反馈）*：早期用「刚好包住图像的小表面 + `set_margin` 定位」，
  拖动时却是**悬浮即拖**（`Enter` 就置了拖动状态、且永不清理），随后改成"按下才拖"仍然
  **时好时坏**：局部指针坐标是相对**当前表面位置**的，而合成器应用 margin 是异步的，
  位置公式 `pos + local - grab` 会重复叠加同一段位移 —— 动快点直接飞出屏幕。
  改成整屏表面后指针坐标就是输出的绝对坐标，`pos = 指针 - 抓取偏移` 幂等且与表面位置无关。
  另外：整屏表面必须 `set_input_region`，否则会挡住下层所有窗口的点击；
  只锚 Top|Left 且 `size=0` 是协议错误（`width 0 requested without setting left and right anchors`），
  四边锚定才能拿到输出尺寸。
- **必须显式指定 output**：layer surface 不绑定 output 时由合成器自己挑（niri 实测挑到了
  另一块屏）。截图侧按「选区中心落在哪个输出」把输出名传给 `pin --output`。
- **缩放 / 透明度**在写 `wl_shm` 缓冲时用 CPU 做（最近邻 + alpha 乘系数），
  不依赖合成器特效；`Argb8888` 是直通 alpha、内存序 B,G,R,A。渲染抽成纯函数
  `pin::paint::render`，像素顺序 / 边框颜色 / alpha 缩放都有单测。
- **输入派发**用**带标记的 udata**（`PinPtr` / `PinKbd`）另开一套 `Dispatch<WlPointer, _>`，
  与覆盖层那份 `Dispatch<WlPointer, ()>` 互不干扰（同一 `Backend` 可以有多个 udata 类型）。
- **生命周期**：贴图要活到用户关掉，而截图会话是"按完就退" → 贴图跑独立进程
  （`airsnip pin <png> [--output]`），截图侧 `spawn` 后自己退出；临时 PNG 由贴图进程读完即删。

### 16. 贴图按 Enter 复制并关闭

与 Windows 版右键「复制」同语义：复制的是 `export_canvas()` —— **当前缩放尺寸的内容**，
不含 1px 边框、不含透明度（复制的是内容，不是显示缓冲）。与截图会话一致，交给 `wl-copy`
持有选区后本进程退出；复制失败则**保持打开**（否则内容直接丢了）。

为此把 `wl-copy` 交接抽到 `crate::clipboard::hand_off`，截图覆盖层与贴图共用。

## 后果

- Windows 侧行为、构建命令、打包产物**均未改变**（根包就是 workspace root）；
  唯一变化是 `editor` 的 46 项测试从 `cargo test` 移到 `cargo test -p airsnip-core`。
- 新依赖（`wayland-client` / `wayland-protocols` / `wayland-protocols-wlr` 均为 MIT，
  `memmap2` MIT/Apache-2.0，`png` MIT）与 GPL-3.0-only 兼容，需登记进 `THIRD-PARTY.md`
  并过 `cargo deny check`（S2 补）。
- Linux 侧没有自动更新（`update/` 依赖 Windows 的进程替换脚本），S5 再决定是否做
  `latest.json` 同构的 Linux 方案，或干脆交给发行版包管理。

## 已知边界（S1 遗留）

1. **多输出缩放不一致**时，按「逻辑坐标 × 该输出缩放」放置是近似；缩放一致（含全 1）时精确。
   实测环境全为 1，暂按精确处理，S2 再决定是否按最大缩放统一栅格。
2. `wl_shm` 用 `/dev/shm` 临时文件（立刻 unlink），文件系统满时抓屏会失败并报错；
   以后可换 `memfd_create`（glibc ≥ 2.27）。
3. S1 的 `capture` 命令会顺手 `encode PNG`（约 200 ms，占单次执行的大头），
   这是**验证路径**才有的成本；S2 起画布直接进覆盖层，不经过编码。
4. 抓帧等待用 `EventQueue::blocking_dispatch` + 自旋上限（4096 次）兜底；
   合成器彻底不响应时会以「超时」报错退出，不会永久卡死。
5. S2 尚无：选区尺寸提示文字、八向手柄拖动缩放、空格+拖动移动选区、选区外单击重选、
   键盘微调。这些都要等 S3 把 cosmic-text 接上（文字）与对象编辑层复用（手柄）后再补。
6. 复制后进程存活到粘贴方取走数据为止（数据源模型使然）；连续复制两次时，第一次的源会被
   `cancelled` 取代，粘贴到的仍是最后一次的内容。
