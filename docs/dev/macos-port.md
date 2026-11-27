# macOS 移植开发规划

> 状态：草案 v2，待评审。代码基线 `next@735d69e`。
> 本文合并了三部分：规划初稿、独立评审（原 `docs/dev/macos-port-review.md`，见 commit `3407fd6`），以及对评审的复核。R1–R9 沿用评审的编号，A 编号沿用初稿。
> 连接方向一律写作「主控 → 被控」。行号以基线为准；行数只用于估量规模。

## 1. 原则

### 1.1 上游要求

上游作者在 [djkcyl/farwing#5](https://github.com/djkcyl/farwing/issues/5#issuecomment-6107018029)（2026-10-11）里说：

> 对于其他平台，我比较看重两点：一是清晰、可维护的后端抽象，让共用业务与平台实现各自有明确的边界；二是实际使用体验，包括编解码、画面呈现的效率，以及输入交互和各项功能的完整性。希望 Linux，以及未来可能支持的 macOS 等平台，都能充分利用各自的原生能力，并在功能行为和操作体验上与 Windows 版尽量保持一致。

这段话的边界：

- 它谈的是 Linux 移植和对未来平台的期望，不是 macOS 合并承诺，也没有指定 PR 数量或 UI 渲染器。
- 作者同时说「你可以先在自己的 fork 中继续迭代，再将适合上游的通用改进整理成 PR」。
- 据评审记录，作者在 Linux PR 的反馈里要求注意平台之间的互联，举的例子正是遗漏了 Mac 键位映射（[comment](https://github.com/djkcyl/farwing/pull/1#issuecomment-6211499564)）；[后续评论](https://github.com/djkcyl/farwing/pull/1#issuecomment-6248328462)要求 Windows 也能用的改进一起做。该 PR 面向旧的 `main`，应借鉴它交叉测试的方法，不要把旧客户端当成兼容对象。

代码里的约定（`src/platform/mod.rs:1-2`）：

> Native OS/driver implementations. Product policy belongs to sessions/features.
> Select the real backend here; no placeholder implementations for future OSes.

### 1.2 本移植的原则

1. **边界清楚。** 产品策略留在 `session` / `features` / `media`，`src/platform/<os>` 只做系统和驱动实现。共用层里不出现任何一个平台的类型（`HWND`、`ID3D11*`、`CVPixelBuffer`……）。
2. **用原生能力，不模拟 Windows。** 不在 macOS 上复刻 named pipe、named mutex、HWND、Win32 虚拟键码这些概念。共用层用中性的概念，各平台用自己的原生机制实现。
3. **效率优先。** 视频走系统硬件编解码，主路径上不把像素读回 CPU；软件编解码只用来兜底。
4. **行为一致。** 功能、设置项、操作语义与 Windows 版对齐；系统惯例层面（窗口按钮、菜单栏、Cmd 键、权限授权）遵循 macOS 习惯。
5. **不写占位实现。** 没有后端的功能，在编译期整条去掉，并且**不对外宣告**；界面上明确标出「macOS 不支持」。不留空函数，也不假装成功。
6. **按能跑通的功能纵向推进。** 不先把全部抽象做完再上 Mac。每一项抽象，在第一个用到它的真实功能里引入。
7. **Windows 不回归，互通改动同版发布。** Windows 已有的行为不能退化。互通需要改 Windows 客户端的地方（§3 的 R1、R2），要和 Mac 版本同时发布：项目不提供跨版本兼容（见 `README.md`）。
8. **权限主体尽早确定。** 从 P0 起就用固定 bundle id、Apple 签发证书签名的 `.app` 来验收；涉及权限的结论，不以 Terminal 或 CLI 下的运行结果为准。

## 2. 现状

| 部分 | 规模 | 平台情况 |
|---|---|---|
| 客户端根 crate（`src/`） | 约 23 万行 | 仅 Windows：`src/lib.rs:3-4` 有 `compile_error!`；`build.rs:2-6` 断言目标必须是 Windows，并嵌入 `assets/windows.rc` |
| `src/platform/windows/` | 约 3.4 万行 | 全部是 Windows 实现 |
| `server/` | 约 3.6 万行 | 已跨平台（axum、rustls、Docker）；但 `records.rs` 有一处平台写死，见 R2 |
| `crates/protocol`、`crates/rtc-wire` | 约 5.5 千行 | 可移植；`Platform` 枚举已包含 `Macos`（`crates/protocol/src/lib.rs:115`） |
| `vendor/webrtc*`、`goog_cc`、`kcp`、`interceptor`、`turn` | — | 纯 Rust，可移植 |
| `vendor/webrtc_neteq` | C/C++ | `build.rs:141-180` 里的 `WEBRTC_WIN`、`winmm` 要按平台切换（见 §11） |
| `vendor/{nvenc,amf,vpl}-sys`、`vendor/egui-directx11` | — | 仅 Windows；`amf-sys` 和 `vpl-sys` 自己带了 `compile_error!`，见 R3 |
| `crates/codec` | 约 7.9 万行 | 见 §2.2 |
| `crates/bootstrap`、`packaging/`、`xtask/`、`drivers/` | — | 仅 Windows |

### 2.1 Windows 耦合分布

在 `src/platform/` 之外，有 126 个文件、约 970 行直接使用 `windows::`：

| 位置 | 文件数 | 说明 |
|---|---|---|
| `features/host` | 37 | 输入注入、显示器、声音、电源、设置、剪贴板 agent、`peer/chat_video.rs` |
| `application/app` | 18 | 主要是 `runtime.rs`、`instance.rs`（单实例）、`maintenance.rs`（进程与 UAC）；`view/**` 基本是纯 egui |
| `application/viewer` | 15 | `windows_keyboard.rs`、`windows_mouse.rs`、`windows_cursor.rs`、`windows_drag.rs`、`windows_ui.rs`、`windows_presenter/**`，约 1 万行 |
| `features/{clipboard,file_transfer,voice,chat,drag_drop}` | 约 15 | 剪贴板 `native.rs`（OLE）、文件路径、语音采集与视频 |
| `transport`、`session`、`account` | 约 15 | 都是小处：路由、地址变化、睡眠唤醒、named mutex、vault、resident 请求 |
| `ui` | 6 | `windows.rs`、`chrome.rs`、`tray.rs`、`video_textures.rs`、`fonts.rs` 等 |

**只搜 `windows::` 还不够**：R1、R2 是写死的 `Platform::Windows` 和静态能力集合，搜 `windows::` 找不到。

可以直接复用的部分：

- 控制中心界面（`application/app/view`）、`ui/controls`、主题、i18n 的大部分；
- transport（FEC、拥塞控制、FCT、NAT 端口映射）、session、account 的 HTTP 部分；
- 音频链路（NetEq、Opus、cpal、SpeexDSP、RNNoise）；
- `media` 下的编码策略、码率、格式选择与码流解析。

### 2.2 编解码 kernel 在 Apple Silicon 上的情况

- `kernels/h264`：能在 aarch64 上编译。SIMD 只有 `x86_64` 分支，aarch64 走标量路径，**没有 NEON**，性能要实测。注意 `farwing-codec` 目前只在 `cfg(windows)` 依赖里（`Cargo.toml:91`），不调整的话，Mac 构建里根本没有软件兜底。
- `kernels/av1`：裁剪过的 rav1d，`lib.rs:8` 只在 x86/x86_64 下导出 decoder，依赖 40 个 NASM 汇编文件。**aarch64 上没有 AV1 软件解码。**
- `kernels/hevc`：只做语法解析，可移植。
- `crates/codec` 的软件编码器只支持 H.264 4:2:0 8-bit。

### 2.3 历史上的 Apple 代码

commit `572014b`（v0.3.0）删除了 `src/decoder/platform/apple/videotoolbox/`（约 1.3 千行，H.264/HEVC 的 VideoToolbox 解码）和 Linux 的 VA-API 实现。现在的架构已经不同，不能直接恢复；但 P2 可以参考其中构造 format description 和 HEVC 配置的写法（`git show 572014b^:src/decoder/platform/apple/videotoolbox/video.rs`）。

## 3. 阻塞互通或阶段验收的问题（R1–R4）

### R1 · 主控按对端平台过滤键盘

**代码事实：**

- `src/features/remote_input.rs:419-434`：`keyboard_supported()`、`supports_system_commands()`、`system_command()` 都要求对端是 `Platform::Windows`。
- `:462`：`key()` 遇到非 Windows 对端直接返回 `false`，按键不会入队。
- `:499`：`sync_locks()` 也一样，锁定键状态不同步。
- 这里比较的平台来自对端注册的身份：经服务器时取 `device.platform`（`session/controller/connection.rs:98,351`、`controller/mod.rs:283`）；局域网直连时取 `host.platform`（`controller/direct.rs:373`）。
- 主控端用来过滤 HID usage 的表在 Windows 平台目录里：`remote_input.rs:15` 引用 `platform::windows::input::keyboard`，`:458` 用 `keyboard::scan(usage)` 过滤。这张「HID usage ↔ set-1 扫描码」表本身已覆盖 ISO、JIS、LANG1/2 等键位。
- 网页主控不受影响：`server/web/src/lib/input.ts` 不按对端平台过滤，`server/web/src/lib/devices.ts:8` 只看对端有没有 `host` 能力。
- 同类的平台判断：
  - `application/app/view/device_details.rs:513,517`：诊断、摄像头入口只对 Windows 显示；
  - `diagnostics/remote/service.rs:106`：远程诊断要求 Windows；
  - `connection.rs:125,348`：远端升级只对 Windows 开放；
  - `connection.rs:57,223`：升级后重连时断言对端仍是 Windows（只在这条路径上触发）。

**影响：** Windows → Mac 时，普通按键、锁定键、系统命令都发不出去。在 Mac 上用 `CGEventPost` 注入，或改本地快捷键的存储方式，都修不了这个问题。修复必须改 **Windows 客户端**，并且和 Mac 版本同时发布。反过来，如果在 Mac 构建里把 Windows 系统命令整个编译掉，又会误删「Mac 主控操作 Windows 被控」时的系统命令菜单。

**修改方向（A21）：** 三类按键分开判断。

- 普通 HID 键：看对端会话里声明的能力，不看平台。具体用哪个 feature 名，要和上游对齐。
- 锁定键同步：看对端平台的语义。macOS 没有 NumLock，CapsLock 的处理参照网页端（`docs/protocol/web-control-brief.md` §8）。
- Windows 系统命令：看对端是否是 Windows，与本机是什么平台无关。

HID 表移到共用层，各被控把 HID usage 转成自己的本地键码。

**验收：**

- 用真实上报 `macos` 的设备，分别测 Windows → Mac、Mac → Windows、浏览器 → Mac，每个方向都要走服务器和局域网直连两条路径。
- 测试项：左右修饰键、CapsLock、窗口失焦或断线后释放按键；Mac → Windows 方向另测可选的 Cmd/Ctrl 映射和系统命令菜单。
- 不能用「把 Mac 报成 Windows 后能用」通过验收。

**判别测试：** 按基线代码，同一台 Mac 被控在两条路径上的结果会不同：经服务器时普通键被丢弃；经局域网时，Mac 报的身份是 Windows（见 R2），按键碰巧能发出去。这说明 R1 必须和 R2 一起修。

### R2 · 平台身份与能力声明分散在多处

| 位置 | 基线行为 | 后果 |
|---|---|---|
| `account/auth/mod.rs:287` 设备注册 | 写死 `Platform::Windows` | 账号设备列表里显示成 Windows |
| `transport/direct/lan.rs:49` LAN 公告 | 写死 | 局域网里显示成 Windows |
| `transport/direct/mod.rs:310` `DirectHello` | 写死 | 直连握手的身份错误 |
| `transport/direct/host.rs:420` `DirectAuthorized`、`:600` `DirectPaired` | 写死 | 同上 |
| `session/controller/direct.rs:161` `DirectRecord.controller_platform` | 写死 | 连接记录里的主控平台错误 |
| `account/feature_ability.rs:36-49` 注册能力 | 固定上报 `host/files/ports/power/wake_on_lan/assist` | 只有主控功能的构建，也会被当作可以连接的被控 |
| `protocol/peer.rs:273-295` 被控 HELLO 能力 | 静态集合包含 `DISPLAY_VIRTUAL`、`CLIPBOARD_FILES`、`ANNOTATION`、`VIDEO_RECOVERY`，并调用 `platform::windows::input::pointer` | 对端会展示未实现的功能；只在本机隐藏按钮没有用 |
| `account/api/mod.rs:1047` 更新查询 | 请求 Windows 的更新 | 可能向 Mac 推荐 Windows 包 |
| `server/src/records.rs:231` 直连记录 | 没有旧记录、且主控先上报时，被控平台写成 `Platform::Windows`；被控之后再上报，只更新 `host_user_id` 和 `host`（`:252-259`），`host_platform` 只在插入时写一次（`:105,116`） | 只要主控先上报，这条记录的被控平台就永久是 `windows` |

`protocol/peer.rs` 里 `controller_features()` 声明的 `DISPLAY_VIRTUAL` 对 Mac 主控是正确的：Mac 被控不支持虚拟屏，不代表 Mac 主控不能请求 Windows 被控创建虚拟屏。

**修改方向：** 明确区分三件事，分别由不同的地方提供。

- 「我是哪个平台」：由 `platform` 提供唯一一个函数，上表所有写死的地方都改用它（A19）。
- 「这个构建实现了什么」：由已编译进来的后端决定（A20）。
- 「用户或本次会话允许什么」：沿用现有的策略和授权。

服务端在被控上报时同步更新 `host_platform`（A22）。

在 Mac 更新机制实现之前，Mac 构建不编入更新查询这条路径。

**验收：**

- P1：断言只有主控功能的 Mac 构建，注册时不宣告 `host`。
- P3：逐项对照账号注册、LAN 公告、直连的 `DirectHello` / `DirectAuthorized` / `DirectPaired`、直连记录、双方 HELLO 和远端 UI。

**判别测试：** Mac 被控、Windows 主控走局域网直连。结束后让主控先上报、被控后上报，然后查 `records.host_platform`。基线代码下结果是 `windows`。

### R3 · 缺少真实的 Cargo 与命令入口边界

**代码事实：**

- `farwing-amf-sys`、`farwing-vpl-sys` 放在通用 `[dependencies]` 里（`Cargo.toml:32-34`），但：
  - 它们各自在 `src/lib.rs:3` 用 `compile_error!` 要求 Windows x64；
  - `vendor/vpl-sys/build.rs` 无条件编译 `dispatcher/libvpl/src/windows/*.cpp`（其中 `main.cpp` 等 include `<windows.h>`），并链接 `advapi32/dxgi/d3d11/cfgmgr32`。
- 结论：Mac 上 `cargo check` 在依赖层就会失败，还轮不到根 crate。
- 相反，`farwing-codec`、`egui-winit`、`winit` 只在 `cfg(windows)` 依赖里（`Cargo.toml:86-98`）。
- 根 crate 的 `build.rs:2-6` 断言目标是 Windows。
- `src/application/mod.rs:20-70` 的入口无条件调用 Windows 的安装、服务、resident、capture agent、组件管理。
- `main.rs` 的 `#![windows_subsystem]` 在其他目标上会被忽略，**不需要改**（初稿 A16 写错了）。
- 去掉 `compile_error!`、改 `.rc`、换 NetEq 宏定义，都不会让 `login` / `devices` 自动变成能在 Mac 上构建的功能。

**修改方向（A18）：**

1. 依赖分层：AMF、VPL、NVENC 移到 Windows 专属依赖；`farwing-codec`、egui、winit 移到通用依赖。
2. 模块导出按 `target_os` 分支。
3. CLI 的 `Commands` 按平台注册，没有后端的命令整条不编入。

**验收：**

- Mac CI 至少跑 `cargo check --locked --target aarch64-apple-darwin`；阶段验收要做原生链接和实际运行。
- Windows 保留 `cargo build --release` 和对应的回归测试。

**判别测试：** 在基线上运行上面那条 `cargo check`，预期停在 `build.rs` 的断言，或 `farwing-amf-sys` / `farwing-vpl-sys` 的 `compile_error!`。这台开发机还没装 Rust，这条尚未实际运行。

### R4 · 发布包可以晚做，权限主体不能晚定

**事实（[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)，"macOS considerations" 一节）：**

- 从 macOS 15 起，访问本地网络需要用户授权。
- 以下三类会被自动放行：`launchd` daemon、以 root 运行的程序、"Command-line tools run from Terminal or over SSH, including any child processes they spawn"。也就是说，**从 Terminal 测试永远看不到被拒绝的情况。**
- `launchd` agent 不在豁免之列。
- 本地网络权限按代码签名识别程序，ad-hoc 签名识别不可靠，要用 Apple 签发的签名身份。
- macOS 15.0 有不少相关 bug，要在 15.1 及以上测试。
- 只要目的地址在本地网段，就属于本地网络操作，不论用的是哪种 API（BSD socket 也算）。所有组播地址和 255.255.255.255 都算本地网络地址。

**对 Farwing 的影响：** 不只是 LAN 发现（`transport/direct/lan.rs` 的组播和广播），还包括：

- 部署在局域网地址上的**自建服务器**：登录和信令都受影响，所以 P1 的验收也会被波及；
- WebRTC 连向局域网对端的 host 候选地址；
- WOL 魔术包。

被拒绝时要给出对应的提示，不能笼统显示成「对端离线」或「服务器不可达」。

录屏、麦克风等 TCC 权限也按「负责进程」归属；从 Terminal 启动时，可能记在 Terminal 名下。这一点是经验行为，要在 P0 验证。

**修改方向：**

- P0 就做一个最小的 `.app`：固定 bundle id、Apple 签发的开发签名，`Info.plist` 写好各项用途说明（见 §6.5）。公证、DMG、自动更新仍然放到 P5。
- 主控在窗口获得焦点时，用普通键盘事件就能工作；全局拦截系统快捷键是可选功能（§6.3）。基础观看不申请「输入监控」。

**验收：** 在干净的用户账号下，从 Finder 启动，测首次允许、拒绝、撤销、重启后的行为：

- 拒绝麦克风：仍然可以观看；
- 允许录屏、拒绝输入控制：只能被观看；
- 拒绝本地网络：给出对应提示，不影响非局域网的路径。

**判别测试：** 服务器放在 192.168.x 地址上，同一个已签名的二进制分别从 Finder 启动（拒绝本地网络权限）和从 Terminal 启动。前者登录失败、后者登录成功，就证明权限验收必须基于 `.app`。

## 4. 目标结构

```
src/platform/
  mod.rs            # 按 target_os 选择后端，对外导出同一组模块名
  windows/          # 现有实现
  macos/            # 新增：通过 objc2 系列 crate 调用系统框架
```

- `platform/mod.rs` 维持「一组模块名，各平台各自实现」的形式：
  - 现有的 `camera, capture, decoder, display, display_hdr, encoder, graphics, source, surface, swapchain, transfer, video_shader`；
  - 需要新增的 `clipboard, drag_drop, ipc, input, tray, notifications, file_locations, power, net, identity`。
- 平台没有实现的模块不导出。调用方在编译期去掉相应功能，同时不对外宣告（R2）。
- macOS 代码通过 `objc2`、`objc2-app-kit`、`objc2-metal`、`objc2-screen-capture-kit`、`objc2-video-toolbox`、`objc2-core-media`、`objc2-core-video`、`objc2-io-surface` 等调用系统框架，不引入 Swift 或 Objective-C 源文件；确有 API 必须这样做时再单独评估。

## 5. 共用层改造清单

每一项都是独立的提交，并在 Windows 上回归。「阶段」列表示**第一个需要它的功能**所在的阶段，到那时才做，不要求在动 Mac 之前全部完成。

| 编号 | 现状 | 目标 | 阶段 |
|---|---|---|---|
| A1 | `media/decode_api/video.rs:31` `gpu_device: Option<ID3D11Device>` | 改为 `platform` 定义的设备句柄 | P2 |
| A2 | `media/decoder/mod.rs:31,52,57,67` `DecodedFrame.surface: D3D11Surface`、`DecoderCandidate::WindowsD3d11`；`decoder_pool.rs:34`；`decoder/diagnostics.rs` 直接探测 D3D11 | 解码输出改为 `platform::surface::Surface`（Mac 上是 IOSurface 支撑的 `CVPixelBuffer`）；候选解码器按平台枚举；诊断移到 `platform` | P2 |
| A3 | `media/decoder/software_slot.rs` 用 named mutex，约定是：同一登录会话、同一 profile 的**所有进程**共用一个软件解码槽（`:1-2`） | 保留跨进程排他。Mac 上对 profile 目录下的锁文件加 `flock`：进程异常退出时内核会自动释放。（初稿写「改成进程内锁」是错的） | P2 |
| A4 | `platform::source::FrameSource::device() -> &ID3D11Device`，`Frame.texture: ID3D11Texture2D`；`Encoder` 的入口都接收 `&ID3D11Texture2D`；`features/host/peer/pipeline.rs` 直接持有 `ID3D11Device` | 采集帧和编码输入改为平台帧类型；`pipeline.rs` 只处理中性的帧和「设备已变化」事件 | P3 |
| A5 | 通用代码里约 95 处把 HWND 当作窗口身份（如 `windows_mouse.rs`、`windows_drag.rs`、`player.rs`） | 引入不透明的 `WindowId`，原生句柄只在 `platform` 里取 | P2 |
| A6 | `application/viewer_shortcuts.rs:54-57` `Binding.key: u16` 是 Win32 虚拟键码，已写进用户配置；按键显示名用 `GetKeyNameTextW` | 改存 HID usage，读取旧配置时迁移；键名由平台提供 | P2 |
| A7 | `i18n/system.rs` 把 Windows 专属文案和通用文案混在一个 struct；`i18n/mod.rs:113` 调用 `GetUserDefaultUILanguage` | 拆成通用文案和各平台文案；系统语言由平台提供 | P1 |
| A8 | `ui/windows.rs` 同时负责通用多窗口管理和 D3D 设备、DWM 设置；`ui/chrome.rs` 依赖 DWM 和 `WM_NCCALCSIZE` | 拆成通用窗口循环 + 平台窗口工厂和呈现器；自绘标题栏的移动和缩放改为平台接口 | P1 |
| A9 | `features/clipboard` 直接调用 `native::*`；`features/drag_drop/mod.rs:8` 写死 `platform::windows::drag_drop as native` | 定义平台接口，保留延迟提供数据和文件承诺的语义 | P4 |
| A10 | 单实例、托盘、通知、实例间消息共用一个 HWND + `WM_COPYDATA`（`application/app/instance.rs`、`ui/tray.rs`） | 抽出「单实例 + 本地消息」接口 | P1 |
| A11 | IPC 是 named pipe 上的 JSON（`host_service/pipe.rs`）；`session/presence.rs:143-154` 用 named mutex；`host_service/wake.rs` 用 named event | 抽出本地 IPC、跨进程锁、唤醒三个接口，帧格式不变 | P1（presence）/ P5（服务） |
| A12 | `product.rs` 用 `PCWSTR`，`EXECUTABLE_NAME = "Farwing.exe"`，pipe 和内核对象的名字都在这里 | 产品标识保持共用；平台相关的名字交给 `platform` | P1 |
| A13 | `transport/nat/route.rs`（`GetBestRoute2`）、`nat/discovery.rs`、`signal/mod.rs`、`direct/lan.rs` 调用 Windows 的路由、地址变化、睡眠唤醒接口 | 收拢到 `platform::net` 和 `platform::power` | P1 |
| A14 | `account/auth/{secret,mod}.rs` 约 15 处调用 `host_service::vault`；`device_init_request` 接收 `platform::windows::device_profile::Hardware` | 凭据存储和硬件信息改为平台接口；用户级凭据用 `keyring`（Mac 上是 Keychain） | P1 |
| A15 | 插件：`plugins/mod.rs:353`、`video.rs:202` 用 `load_with_flags` 加载；`plugins/process.rs` 用 Job Object；`plugins/metadata.rs:39-51` 读 `.fwmeta` section，`:87-99` 扫描时只认 `.dll`；`object` crate 只开了 `pe` feature（`Cargo.toml:73`）；视频插件只接受 `d3d11-rgba16f` 和 `DXBC`（`video.rs:114-128`） | 普通插件：Mach-O 元数据（section 命名形如 `__DATA,__fwmeta`，最长 16 个字符）+ 签名策略；视频插件需要 Metal shader 的表示。没有实现的后端明确拒绝加载 | P4 |
| A16 | `build.rs:2-6` 只认 Windows；`lib.rs:3-4` 有 `compile_error!`；NetEq 使用 `WEBRTC_WIN`、链接 `winmm` | 按 `target_os` 分支；Mac 上不嵌入 `.rc`，NetEq 用 `WEBRTC_MAC` / `WEBRTC_POSIX` | P1 |
| A17 | `ui/fonts.rs:67-71` 从 `%WINDIR%\Fonts` 读字体 | 字体查找交给平台（PingFang SC 等，可通过 CoreText 查找） | P1 |
| A18 | R3：依赖、模块导出、CLI 入口 | 见 R3 | P1 |
| A19 | R2：平台身份写死在 8 处 | `platform::identity` 提供唯一的来源 | P1 |
| A20 | R2：注册能力和 HELLO 能力是静态集合 | 由已编译的后端生成；只有主控功能的构建不宣告 `host` | P1（主控）/ P3（被控） |
| A21 | R1：主控按平台过滤按键；HID 表在 `platform::windows` | 按三类分别判断；HID 表移到共用层。**这项要改 Windows 客户端** | P2（Mac 侧）/ P3（Windows 侧） |
| A22 | R2：`server/src/records.rs` 的 `host_platform` 写入后不再更新 | 被控上报时同步更新 | P3 |

## 6. macOS 后端映射

### 6.1 视频与呈现

**数据路径：**

- 被控发送：`SCStream → CVPixelBuffer → VTCompressionSession → 压缩码流`
- 主控接收：`压缩码流 → VTDecompressionSession → CVPixelBuffer → CVMetalTextureCache → Metal`

IOSurface 是像素缓冲的底层存储，不是编码器的输出，也不决定权限和进程归属。目标写作「硬件主路径不把像素读回 CPU」，不写「全链路零拷贝」。

| 能力 | Windows（现状） | macOS（目标） | 说明与待验证 |
|---|---|---|---|
| 屏幕采集 | DXGI Desktop Duplication、GDI 兜底、特权 capture agent | ScreenCaptureKit `SCStream` | 同一进程内不需要跨进程传纹理的 agent |
| 采集格式 | BGRA8 / FP16 → NV12、P010、AYUV、Y410（D3D 着色器） | SDR 先用 `420v` / `420f`；4:4:4 10-bit 可考虑 `xf44` | SDK 15.5 的 `SCStream.h:222-229` 只列出 `BGRA`、`l10r`、`420v`、`420f`、`xf44`、`RGhA`，**没有** 10-bit 4:2:0（P010）。HDR 从 `SCStreamConfigurationPresetCaptureHDR*` 预设（macOS 15）开始，记录实际得到的格式和色彩元数据，只在必要时用 Metal 转换。见 [pixelFormat](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/pixelformat)、[WWDC24 10088](https://developer.apple.com/videos/play/wwdc2024/10088/) |
| 窗口采集 | Windows.Graphics.Capture | SCK `SCContentFilter(desktopIndependentWindow:)` | |
| 锁屏状态 | `WTSQuerySessionInformationW` | `CGSessionCopyCurrentDictionary` | 返回的是会话信息；「已登录」不等于「屏幕未锁定」（R9） |
| 硬件编码 | NVENC、AMF、QSV（约 3.7 千行） | `VTCompressionSession`：H.264、HEVC（含 Main10） | VT 不能编码 AV1；4:4:4 和 10-bit 支持随机型变化，运行时探测 |
| 长期参考恢复 | `FrameControl{mark_ltr, use_ltr}` 指定槽位，`LtrOutput{marked, used}` 报告实际使用的槽（`media/encoding/mod.rs:193-208`） | VT 的 token 模型 | 见 R6。首版走 IDR 兜底，**不宣告** `video.recovery` |
| 软件编码 | `software_encoder.rs` 回读 BGRA 后交给 `farwing_codec` | 锁定 `CVPixelBuffer` 后交给 `farwing_codec` | 只作兜底；aarch64 上是标量路径 |
| 硬件解码 | 自写的 DXVA11（`decoder/rust_dxva/`） | `VTDecompressionSession`：H.264、HEVC | 要验证遇到恢复帧和 `frame_num` 跳号时的行为（§9 V3）；可参考 `572014b` |
| AV1 解码 | DXVA；x86 软件解码 | 先查 `VTIsHardwareDecodeSupported`，再按目标 profile、位深、分辨率实际创建 session | 「M3 及以上」不是对所有格式的保证；aarch64 上没有软件兜底 |
| 解码输出 | `D3D11Surface`（共享 NT handle + keyed mutex） | IOSurface 支撑的 `CVPixelBuffer` | 使用期间要持有 `CVMetalTexture`（见 SDK 头文件 `CVMetalTextureCache.h`），确认 GPU 用完之前缓冲区不会被复用，并限制在途帧数 |
| egui 渲染与视频上屏 | `vendor/egui-directx11` + DirectComposition；视频放在独立子 HWND 上，由单独线程呈现 | **待 R7 对比测试后决定** | 通话画面通过 egui 纹理进入多个窗口（`ui/video_textures.rs:1-4`），所以只演示一个独立视频窗口不够 |
| HDR 显示 | `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`（`display_hdr.rs`） | EDR（`wantsExtendedDynamicRangeContent`）、`kCGColorSpaceITUR_2100_PQ`，显示能力查 `NSScreen` | |
| 显示器拓扑 | `EnumDisplayMonitors`、`QueryDisplayConfig` | `CGGetActiveDisplayList`、`CGDisplayMode`、`SCShareableContent` | |
| 光标 | `GetCursorInfo`、`DrawIconEx` | SCK `showsCursor`；光标形状来源待验证 | 需要确认能否拿到其他应用的光标形状 |
| 线程优先级 | `gpu_scheduling.rs` | 线程 QoS `QOS_CLASS_USER_INTERACTIVE` | |

### 6.2 声音与摄像头

| 能力 | Windows | macOS | 说明 |
|---|---|---|---|
| 桌面声音 | WASAPI loopback；Windows 20348 及以上排除设备中心整棵进程树的声音，无论设备中心是否在后台服务里（commit `3bc8575`） | 应用内被控：SCK `capturesAudio` + `excludesCurrentProcessAudio`（macOS 13，`SCStream.h:307`） | 被控拆成独立进程（P5）或需要按进程采集时，再用 Core Audio tap（`AudioHardwareCreateProcessTap`，macOS 14.2+，需要 `NSAudioCaptureUsageDescription`） |
| 单个进程的声音 | `ActivateAudioInterfaceAsync` | Core Audio tap 指定进程 | 同上 |
| 播放与麦克风 | cpal（WASAPI） | cpal（CoreAudio） | `features/voice/engine.rs:164` 的 `HostId::Wasapi` 改为按平台选择 |
| 虚拟声卡 | Farwing Audio 驱动 | AudioServerPlugIn（BlackHole 的做法，不需要特殊授权） | P4 之后 |
| 摄像头 | Media Foundation、`IAMVideoProcAmp` | `AVCaptureSession`、`AVCaptureDevice` 的属性 | `camera/synthetic.rs` 可复用 |
| 摄像头提示浮窗 | 分层窗口 | `NSPanel` | |

验收：分别验证「远端能听到目标应用」和「远端听不到自己的回声」。

### 6.3 输入

| 能力 | Windows | macOS | 说明 |
|---|---|---|---|
| 主控普通键盘 | 低级键盘钩子（`windows_keyboard.rs`，窗口在前台时始终安装） | 窗口获得焦点时的 NSEvent / winit 键盘事件，不需要 TCC 授权 | 按物理键位转成 HID usage |
| 主控拦截系统快捷键（Cmd-Tab、Cmd-Space 等） | 同一个钩子 | 可选功能，用 `CGEventTap` 实现 | 需要哪项权限待验证（§9 V6） |
| 相对鼠标 | raw input、`ClipCursor` | `CGAssociateMouseAndMouseCursorPosition(false)` + `CGWarpMouseCursorPosition`，从事件里取增量 | |
| 远端光标形状 | `LoadCursorW` / `CopyImage` | `NSCursor` + `CGImage` | |
| 被控输入注入 | SendInput / 输入驱动 | `CGEventPost(kCGHIDEventTap)`，需要「辅助功能」权限 | 触控和笔没有对应的注入方式 |
| Cmd / Ctrl | — | 提供「Cmd 映射为 Ctrl」选项，与网页端一致 | |
| 锁定键 | `LockState` | macOS 没有 NumLock；CapsLock 的处理参照网页端 | 见 R1 |

### 6.4 窗口、系统集成与进程

| 能力 | Windows | macOS |
|---|---|---|
| 事件循环与窗口 | winit + Win32 扩展 | winit（AppKit 后端）；通过 `raw-window-handle` 取 `NSView` |
| 标题栏 | 自绘 + DWM | `NSWindow` 透明标题栏 + `fullSizeContentView`，保留红绿灯按钮 |
| 菜单栏 | 无 | 最小的应用菜单（关于、设置、退出、编辑），保证系统快捷键可用 |
| 托盘 | `Shell_NotifyIcon` | `NSStatusItem` + `NSMenu` |
| 通知 | WinRT Toast | `UNUserNotificationCenter`；需要定位的自绘浮窗用 `NSPanel` |
| 文件对话框 | `file_dialog.rs` | `NSOpenPanel` / `NSSavePanel`（`rfd` 底层就是这两个 API） |
| 剪贴板 | OLE，延迟渲染 | `NSPasteboard` + `NSPasteboardItemDataProvider` + 文件承诺；不用 `arboard`（它不支持延迟提供和文件承诺） |
| 拖放 | OLE | `NSDraggingDestination`、`NSDraggingSession`、`NSFilePromiseProvider` / `NSFilePromiseReceiver` |
| 批注与参与者浮层 | 分层、鼠标穿透的窗口 | `NSPanel` + `ignoresMouseEvents` |
| 单实例与本地消息 | named mutex + `WM_COPYDATA` | 锁文件 `flock` + Unix socket |
| 本地 IPC | named pipe + JSON | Unix domain socket + 相同的 JSON 帧；用 `LOCAL_PEERCRED` / audit token 校验 uid，用 `SecCode` 校验签名 |
| 用户数据 | `%LOCALAPPDATA%\Farwing` | `~/Library/Application Support/Farwing`；日志在 `~/Library/Logs/Farwing` |
| 用户级凭据 | `keyring`（凭据管理器） | `keyring`（Keychain） |
| 防止休眠 | `SetThreadExecutionState` | `IOPMAssertionCreateWithName` |
| 睡眠与唤醒 | `host_lifecycle.rs` | `NSWorkspace` 的 will-sleep / did-wake 通知，或 `IORegisterForSystemPower` |
| 路由、MTU、地址变化 | `GetBestRoute2`、`GetAdaptersAddresses`、`NotifyIpInterfaceChange` | `getifaddrs`、`SIOCGIFMTU`、`PF_ROUTE` 或 `SCDynamicStore`；源地址用 UDP `connect()` 探测。不要写死 `en0`（TN3179） |
| 网络唤醒 | `wol.rs` | 同样的魔术包；MAC 地址取自 `getifaddrs` 的 `AF_LINK` 项 |
| 自启动 | `HKCU\...\Run` | `SMAppService.mainApp`（登录项） |

**无人值守（R9）：**

- 「前台只有一个控制台用户」不等于「只有一个已登录的图形会话」：快速用户切换时，可以同时有多个已登录的会话。初稿「按会话分发的逻辑可以大幅简化」的说法是错的。
- [`SMAppService`](https://developer.apple.com/documentation/servicemanagement/smappservice) 注册的 LaunchAgent 按用户生效；LaunchDaemon 需要管理员批准。daemon 有 root 身份，但不等于拥有某个用户的录屏和输入授权。
- 首版被控只承诺「已登录、当前活动、并且已经授权的用户桌面」。
- 交互式共享可以评估 [`SCContentSharingPicker`](https://developer.apple.com/documentation/screencapturekit/sccontentsharingpicker)。[Persistent Content Capture entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.persistent-content-capture) 面向 VNC 类应用，需要单独向 Apple 申请，Farwing 目前没有。
- 验收：分别测切换用户、锁屏、注销、重启后尚无人登录。不能把上一位用户的画面或输入接给下一位用户；不支持的状态要明确停止串流，不能因为「服务仍在线」就判为成功。

### 6.5 权限

| 权限 | 用途 | 申请方式与 `Info.plist` |
|---|---|---|
| 屏幕录制 | 被控采集画面 | `CGRequestScreenCaptureAccess`；授权后通常要重启应用 |
| 辅助功能 | 被控注入键鼠 | `AXIsProcessTrustedWithOptions`，然后引导用户去系统设置 |
| 输入监控 | 仅用于可选的拦截系统快捷键 | `CGRequestListenEventAccess`（是否足够见 §9 V6） |
| 麦克风、摄像头 | 语音、视频通话、麦克风转发 | `AVCaptureDevice.requestAccess`；`NSMicrophoneUsageDescription`、`NSCameraUsageDescription` |
| 系统音频录制 | Core Audio tap（P5） | `NSAudioCaptureUsageDescription` |
| 本地网络（macOS 15+） | LAN 发现、局域网直连、局域网里的自建服务器、ICE host 候选、WOL | 首次访问时由系统弹窗；`NSLocalNetworkUsageDescription` |

界面上要有一个权限检查页，列出每项权限的状态和「去设置」按钮，放在与 Windows 组件检查相同的位置。

## 7. macOS 不支持或暂缓的功能

| 功能 | 原因 | 处理 |
|---|---|---|
| 安全桌面、Ctrl+Alt+Del、登录界面控制 | 登录窗口属于 `loginwindow`，没有可以注入的独立桌面 | Mac **被控**不编入、不宣告；Mac **主控**操作 Windows 被控时保留这些命令（R1） |
| 虚拟显示器 | 只有私有 API `CGVirtualDisplay`；DriverKit 显示驱动需要 Apple 单独授权 | Mac 被控不宣告 `DISPLAY_VIRTUAL`；Mac 主控仍可以请求 Windows 被控创建 |
| HID 级输入驱动 | DriverKit HID 需要 Apple 授权 | 用 `CGEventPost` |
| 触控与笔的注入 | 没有公开 API | Mac 被控不宣告；Mac 主控是否发送触控和笔，按对端的声明决定 |
| NVENC / AMF / QSV | 不适用 | 不编译 |
| 虚拟声卡 | 要写 AudioServerPlugIn | P4 之后 |
| 驱动安装与组件管理 | 不适用 | 不编译 |
| 长期参考恢复（被控侧） | VT 的模型与 Farwing 不同（R6） | 首版不宣告 `video.recovery`，用 IDR 兜底 |

## 8. 阶段计划

阶段编号与初稿的对应：初稿的 P1「前置抽象」拆散到各阶段里（见 §5「阶段」列）；初稿的 P2–P6 依次改为这里的 P1–P5。

### P0 准备

- 在 Mac 上安装 Rust（与项目一致，当前 1.95）和 Xcode 命令行工具。
- 准备 Windows 回归环境（Windows 机器或 CI）。
- 申请或确认 Apple Developer 账号。
- 做一个最小的已签名 `.app` 骨架：固定 bundle id、Apple 签发的开发签名、§6.5 列出的用途说明。
- 在 Finder 和 Terminal 两种启动方式下，跑通 R4 的判别测试。
- Mac CI：`cargo check --locked --target aarch64-apple-darwin`。开头必然失败，失败项就是 P1 的待办清单。
- 确定 §12 的待决策项。建议在上游开 issue 或讨论，对齐 A19–A21 的形状。

### P1 构建与身份：Mac 能构建、能登录

- A18（依赖、入口、CLI）、A16、A12、A7、A8、A10、A11（presence 部分）、A13、A14、A17。
- A19：所有路径都如实上报 `macos`。A20：只有主控功能的构建，不宣告 `host`。
- 窗口 + egui 渲染：先用能最快跑起来的后端做控制中心，正式选型在 P2 的 R7 对比之后。
- 验收：
  - 签名 `.app` 从 Finder 启动，能登录、看到设备列表，聊天和设置页可用；
  - 账号设备列表、LAN 公告和直连握手里，这台 Mac 的平台都是 `macos`，并且不宣告 `host`；
  - 服务器放在局域网地址上时，本地网络授权、拒绝两种情况的行为都正确；
  - Windows 构建和回归通过。

### P2 主控：Mac → Windows

1. **第一个可用版本：** H.264 SDR 1080p60，键鼠（A5、A6、A21 的 Mac 侧），断线重连。用到 A1、A2、A3。
2. **R7 渲染对比**，然后定下维护哪条路径。
3. **扩展：** 4K60、HEVC、能力不足时的协商与降级；VT 解码遇到恢复帧的行为（§9 V3）。
4. 声音播放、麦克风转发、托盘、通知、文件对话框。

同时做一个小规模的 SCK → VT 被控采集试验，提前暴露采集格式和授权问题。这只是试验，不等于提前交付被控。

验收：按 §10 的性能门槛，对照 `docs/manual-test-checklist.md` 里主控相关的条目。

### P3 被控：Windows → Mac、浏览器 → Mac，最后 Mac → Mac

- A4；ScreenCaptureKit 采集（多显示器、窗口）；VT 编码 H.264 / HEVC，用 IDR 兜底。
- `CGEventPost` 注入；SCK 采集声音并排除本进程；`IOPMAssertion` 防休眠；权限检查页。
- A20 的被控部分：只宣告已经实现的功能。
- A21 的 Windows 侧和 A22：与 Mac 版本同时发布。
- 只做应用内被控（对应 Windows 的便携被控，不装服务）。
- 验收：
  - R1、R2 的全部验收项；
  - 浏览器 → Mac 可以先于 Windows 侧的修复进行（见 R1）；
  - Mac → Mac 在两端都实现后再测。

### P4 功能补齐

- 剪贴板（含延迟提供和文件）、拖放、文件传输（A9）。
- 摄像头、语音与视频通话、批注与参与者浮层、多人。
- 端口转发、网络唤醒、远程电源操作。
- HDR（V2）、显示设置；插件（A15）。
- 被控侧长期参考恢复（V4 通过后才宣告 `video.recovery`）。
- 虚拟声卡（AudioServerPlugIn），视进度决定。

### P5 发布、更新与无人值守

- 完整的 `.app`、hardened runtime + entitlements、`codesign`、`notarytool`、`.dmg`。插件需要考虑 hardened runtime 默认开启的 [library validation](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.disable-library-validation)。
- 更新机制：替代现有的 PE 自解压和 `components/application.rs`，可以用 Sparkle 2，也可以基于服务端的更新公告接口自己实现。在这之前，Mac 不编入更新查询路径（R2）。
- 开机自启（`SMAppService` 登录项）。
- 无人值守：按 §6.4 R9 的边界实现；被控拆成独立进程后改用 Core Audio tap 排除声音；补充机器级凭据。
- 在 `docs/dev/identity.md` 加上 macOS 一栏（bundle id、目录、socket、Keychain 服务名）。

## 9. 待真机验证的问题

每一项都写明「用最小用例测什么」，以及「结果会改变哪个决定」。

| 编号 | 问题 | 最小用例 | 影响的决定 |
|---|---|---|---|
| V1（R7） | egui 用哪个渲染后端 | 同一份素材，对比三种方案：现成 egui 后端 + 独立原生视频层；自写 egui Metal 后端；以 [AVSampleBufferDisplayLayer](https://developer.apple.com/documentation/avfoundation/avsamplebufferdisplaylayer) 或直接 Metal 呈现作对照。场景至少包括独立播放、叠加 UI、通话缩略图、多窗口关闭与重建。记录显示延迟、丢帧、是否有 CPU 像素回读 | 维护哪条渲染路径。初稿「wgpu 难以导入 IOSurface」说得太绝对：wgpu 可以通过 hal 层从原生 Metal 纹理创建纹理（需要 unsafe，具体版本未核实） |
| V2（R5） | HDR 采集实际得到什么格式 | 用 `SCStreamConfigurationPresetCaptureHDRLocalDisplay` 生成配置，读回 `pixelFormat`、`colorMatrix`、色彩空间，再送进 VT HEVC Main10 | 是否需要 Metal 转换；HDR 的支持范围 |
| V3（R6） | VT 解码器遇到 Windows 恢复帧时的行为 | Windows 被控生成 H.264 / HEVC 恢复帧；在 Mac 端制造缺帧和 `frame_num` 跳号，看画面能否正确恢复 | H.264 是否改走软件解码，或调整恢复策略 |
| V4（R6） | VT 编码器实际依赖了哪个长期参考 | 打开 `kVTCompressionPropertyKey_EnableLTR`；确认第 N 帧的 token；用 `kVTEncodeFrameOptionKey_ForceLTRRefresh` 请求刷新；然后解析输出帧的 slice header（H.264 的长期参考字段、HEVC 的 `lt_idx_sps`），判断实际引用了哪一帧。再加上两个观看者分别确认不同 token 的情况 | 被控能否宣告 `video.recovery`。SDK 头文件 `VTCompressionProperties.h:1161-1217` 说明，刷新时编码器「使用某个已确认的 LTR，没有就输出 IDR」，调用方不能指定槽位，所以不能把请求的槽当成实际引用写进 frame-meta。解析码流也许能证明实际依赖，证明不了就退回 IDR |
| V5（R5） | 在途帧数与缓冲区复用 | 解码 → `CVMetalTextureCache` → 呈现；限制在途帧数，用 Metal 的完成回调检查缓冲区是否被提前复用 | 呈现队列的深度 |
| V6 | 拦截系统快捷键需要哪项权限 | 分别在「只授权输入监控」「只授权辅助功能」「两项都授权」三种情况下，用 `kCGEventTapOptionListenOnly` 和 `kCGEventTapOptionDefault` 创建 tap，看能否吞掉 Cmd-Tab | §6.3、§6.5 的权限写法 |
| V7 | `farwing_h264` 在 aarch64 上的软件性能 | 1080p60 编码和解码的基准测试 | 是否需要写 NEON 优化 |
| V8（R4） | TCC 权限归到哪个进程 | 同一个二进制分别从 Finder 和 Terminal 启动，查看「隐私与安全性」里的授权记在谁名下 | 开发和测试流程 |

## 10. 性能验收门槛

- **对比条件：** 同一台 Windows 被控，同样的码流配置，同样的显示刷新率和网络条件，分别比较 Windows 主控和 Mac 主控。
- **指标：** 使用观看端已有的 summary 字段（`application/viewer/mod.rs:675-706`）`local_p95_ms`、`render_interval_p95_ms`、`dropped_present_frames`，另外测量从丢包到恢复出正确画面的时间。
- **门槛示例：** 60 Hz 下，Mac 的 `local_p95_ms` 最多比 Windows 多一帧（约 16.7 ms）。**这只是待商定的阈值，不是实测结果，也不代表端到端的输入延迟。**

## 11. 风险

| 风险 | 影响 | 应对 |
|---|---|---|
| 共用层改动在 Mac 上无法验证 Windows 行为 | Windows 回归 | 必须有 Windows 回归环境；每一项单独提交、单独验证 |
| 互通修复要改 Windows 客户端（R1、R2） | 旧的 Windows 客户端永远无法向 Mac 输入按键 | 与 Mac 同版发布；尽早和上游对齐 |
| aarch64 上没有 AV1 软件解码 | M1、M2 主控收不了 AV1 | 如实声明能力，让协商自动避开；以后可以从上游 rav1d 引入 aarch64 汇编 |
| `farwing_h264` 在 aarch64 上只有标量路径 | 软件兜底性能不足 | 以 VT 为主；按 V7 的结果决定是否写 NEON |
| VT 的恢复与 LTR 行为不确定（R6） | 丢包后恢复变慢 | 首版用 IDR 兜底；根据 V3、V4 的结果再决定 |
| TCC、本地网络权限与签名（R4） | 测试结果失真；用户首次使用的门槛高 | P0 起就用签名 `.app` 和 Finder 启动来验收；做好权限检查页 |
| 与上游同步 | 抽象改动容易冲突 | 先在上游讨论 A19–A21 的形状，尽量合入上游 |
| 渲染后端选型（R7） | 工作量和维护成本 | 用 V1 的数据决定，不预设结论 |

## 12. 待决策

| 问题 | 建议 |
|---|---|
| 最低系统版本与架构 | 建议 macOS 14+，仅 arm64。基础桌面声音只需要 macOS 13；按进程排除或采集声音要 14.2；HDR 采集和 SCK 采集麦克风要 15；本地网络权限从 15 开始存在，这是约束，不是要求。14.2 是产品策略，不是基础串流的硬门槛 |
| 先做主控还是被控 | 先做主控（P2），同时做一个小规模的被控采集试验 |
| egui 渲染后端 | 等 V1 的结果 |
| 是否与上游合作推进 A19–A21 | 建议先开 issue 或讨论 |
| 更新机制 | P5 时再定 |

## 13. 证据边界

**已核实：**

- 上游作者在 #5 的回复；
- 基线 `735d69e` 中本文引用的所有代码位置；
- Apple 文档 TN3179；
- 本机 macOS SDK 15.5 的 `SCStream.h`（像素格式、`excludesCurrentProcessAudio`、HDR 预设）和 `VTCompressionProperties.h`（LTR）。

评审时还在 arm64 Mac、SDK 15.5 上做过一项检查：按 `vendor/webrtc_neteq/sources.txt` 加上 `src/media/audio/neteq_bridge.cc`，保留 `build.rs` 的通用宏，把 Windows 宏换成 `WEBRTC_MAC` / `WEBRTC_POSIX`，用 `clang -fsyntax-only` 和 `clang++ -std=c++17 -fsyntax-only` 检查了 123 个编译单元，没有发现语法错误。复核时没有重跑这项检查。

**未核实：**

- Mac 客户端的构建与链接（本机尚未安装 Rust），包括 R3 的判别测试；
- TCC 和本地网络权限的实际行为；
- 串流性能；
- wgpu hal 层的互操作；
- event tap 需要的权限；
- VT 解码遇到 `frame_num` 跳号时的行为；
- 作者在 PR #1 里的评论（转引自评审，复核时没有打开原文）。

## 14. 参考

- 上游要求：[djkcyl/farwing#5](https://github.com/djkcyl/farwing/issues/5)
- 已有的 Linux 移植（基于改名为 Farwing 之前的代码）：[dytan/openuuyc](https://github.com/dytan/openuuyc)（`LINUX.md`、`LINUX-HOST.md`），[djkcyl/openuuyc#1](https://github.com/djkcyl/openuuyc/pull/1)
- Apple：[TN3179 本地网络隐私](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)、[Core Audio taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps)、[excludesCurrentProcessAudio](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/excludescurrentprocessaudio)、[WWDC21 10158 低延迟编码](https://developer.apple.com/videos/play/wwdc2021/10158/)、[SMAppService](https://developer.apple.com/documentation/servicemanagement/smappservice)
- 本仓库：`docs/protocol/farwing-protocol.md`（平台枚举）、`docs/protocol/rfi-ltr-brief.md`（恢复帧）、`docs/protocol/web-control-brief.md` §8（macOS 键盘差异）、`docs/dev/identity.md`
