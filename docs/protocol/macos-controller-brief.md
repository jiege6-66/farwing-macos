# macOS 主控（Mac → Windows）实现简报

本文是 macOS 原生主控的实现依据：让 Apple Silicon Mac 运行与 Windows 客户端同一份代码的主控（设备中心 + 观看窗口），连接 Windows 被控。首版范围：

- 服务器设置、登录、设备列表、好友与授权（命令行与设备中心）；
- 观看：H.264 / HEVC 硬件解码（具备条件时加 AV1），H.264 软件兜底；
- 键鼠控制、光标、观看窗口快捷键、全屏与多显示器窗口；
- 被控桌面声音的播放、麦克风送往被控；
- 局域网发现与地址直连（作为主控）。

不在本文：Mac 被控、无人值守、虚拟屏与驱动、剪贴板、文件传输、拖放、聊天语音与通话、批注、插件、HDR、自动更新。它们在 §0.2 列为后续项，届时另写简报。

- 基线的 `文件:行` 对应 2026-11-27 的代码快照（`next@735d69e`）；M1 更新的引用对应 `bf70694`，M2 更新的引用对应 `aa1517d`。代码与 `docs/**/*.md` 不一致时以代码为准，差异见 §10。
- 本文写作时没有在 Mac 上构建过本项目（开发机尚未安装 Rust）。标【待测】的结论需要真机验证。
- 用语：被控 = host；主控 = controller = 本文的 Mac 客户端；“Windows 主控”指现有的 Windows 客户端；“平台层”指 `src/platform/<os>`。

上游对其他平台的期望（[djkcyl/farwing#5](https://github.com/djkcyl/farwing/issues/5#issuecomment-6107018029)，原帖讨论的是当时的 Linux 移植；作者表达的是架构与体验上的期望，建议先在 fork 迭代、再把通用改进整理成 PR，并未要求移植先获批准）：清晰、可维护的后端抽象，让共用业务与平台实现各有明确边界；编解码、画面呈现的效率，输入交互和功能完整性；充分利用各平台的原生能力，功能行为和操作体验与 Windows 版尽量一致。代码约定见 `src/platform/mod.rs:1-2`：产品策略留在 session / features，平台层只做系统实现，不写给未来系统的占位实现。由此本文的做法是：

- 没有后端的功能整条不编入，并且**不对外宣告**（不出现在设备注册能力和 HELLO 特性里）；
- 共用层不出现任何平台类型；
- 每一步都在 Windows 上回归，Windows 行为不变。

整体背景（含被控与后续阶段的评审意见）见 commit `2dfefe0` 中的 `docs/dev/macos-port.md`。

与上游的协调：2026-11-28 开了 [djkcyl/farwing#34](https://github.com/djkcyl/farwing/issues/34)，问两件事：(1) 只做主控的平台上 `features::host` 的边界放在哪里，是沿用 Linux 主控 [PR #33](https://github.com/djkcyl/farwing/pull/33) 的整块按 Windows 编入，还是先提小 PR 把共享状态与被控运行时分开（§2.2）；(2) 上游 [`macos-viewer`](https://github.com/djkcyl/farwing/tree/macos-viewer) 分支的窗口与呈现实现是否计划迁到 `next`（§4.2、§5.6）。回复前不动 `features::host` 和后台服务分支；不依赖这两件事的工作（M3 无窗口解码、M4 无窗口输入）照常推进。

---

## 0. 结论速览

### 0.1 现在为什么做不了（阻断项）

1. **构建在依赖层就失败。**
   - `build.rs:2-6` 断言目标是 Windows，并编译 `assets/windows.rc`；`src/lib.rs:3-4` 是 `compile_error!`。
   - `farwing-amf-sys`、`farwing-vpl-sys` 在通用 `[dependencies]`（`Cargo.toml:32-34`），而它们各自在 `src/lib.rs:3` 要求 Windows x64；`vendor/vpl-sys/build.rs` 无条件编译 `dispatcher/libvpl/src/windows/*.cpp`（含 `<windows.h>`）并链接 `advapi32/dxgi/d3d11/cfgmgr32`。
   - 反过来，`farwing-codec`、`egui-winit`、`winit` 只在 `[target.'cfg(windows)'.dependencies]`（`Cargo.toml:86-98`），Mac 构建既没有窗口库，也没有 H.264 软件解码。
   - NetEq 的构建写死 `WEBRTC_WIN` / `NOMINMAX` / `winmm`（`build.rs:141-180`）。
2. **被控代码和主控缠在一起，不能整块去掉。**
   - 登录后的客户端对象带着被控句柄：`AuthenticatedClient.host: features::host::Handle`（`src/account/client/mod.rs:44`、`:111`、`:170`）；控制中心各处经它读写本机被控状态，`src/` 中约 100 处（`grep -E '(client|self|c)\.host\b'`，含少量误报）。
   - 登录和登出时迁移被控设置（`src/account/auth/mod.rs:27-72`）；GUI 启动时开启显示器回退（`src/application/app/mod.rs:55`）。
   - 主控路径用到放在 `features::host` 里的共享类型（§2.3；M1 已迁出，`wake_locally` 与 `chat_video` 除外）。
3. **共用层带着 Windows 类型。**
   - 解码：`VideoDecoderConfig.gpu_device: Option<ID3D11Device>`（`src/media/decode_api/video.rs:31`）；`DecodedFrame.surface` 是 `D3D11Surface`（`src/media/decoder/mod.rs:27-31`）；`DecoderCandidate::WindowsD3d11`（`:66-67`）；`decoder_pool.rs:34` 持有 `D3D11SurfaceWriter`；软件解码槽用 named mutex（`decoder/software_slot.rs`）。
   - 观看窗口入口：session 层直接构造 `application::viewer::windows_presenter::ConnectingWindowsRunConfig`（`src/session/controller/viewing.rs:161-175`）。
   - 输入：主控过滤 HID usage 的表在 `platform::windows::input::keyboard`（`src/features/remote_input.rs:15`、`:458`）；按键超时计时器也在 Windows 平台层（`:309`、`:337`）。
4. **身份和能力写死成 Windows 被控。**
   - 设备注册 `platform: Windows`（`src/account/auth/mod.rs:287`），注册能力固定为 `host/files/ports/power/wake_on_lan/assist`（`src/account/feature_ability.rs:36-49`）。这些能力全是被控能力（`crates/protocol/src/lib.rs:160-171`），Mac 主控如实注册应为空集合，否则服务器把它列为可连接的被控（`src/account/api/mod.rs:392-395`）。
   - 直连握手 `DirectHello.platform` 写死（`src/transport/direct/mod.rs:310`），主控直连记录 `controller_platform` 写死（`src/session/controller/direct.rs:161`）。
   - 主控 HELLO 特性静态包含 `touch`、`pen`、`clipboard`、`clipboard.files`、`files`（`src/protocol/peer.rs:244-259`），首版 Mac 都没有实现。
   - 更新查询固定请求 Windows 的包（`src/account/api/mod.rs:1047`）。

好消息：**Mac → Windows 方向的键鼠不受平台判断阻挡。** `RemoteInput` 只看**对端**是否为 Windows（`src/features/remote_input.rs:419-434`、`:462`、`:499`），对端平台取自设备列表或直连握手（`src/session/controller/connection.rs:98`、`:351`，`src/session/controller/direct.rs:373`）。反方向（Windows → Mac）被挡，属于 Mac 被控的事，见 `2dfefe0` 中的 R1。

### 0.2 各功能一览

| 功能 | Windows 实现 | macOS 做法 | 首版 |
|---|---|---|---|
| 服务器、登录、设备、好友、授权（CLI） | `src/main.rs`、`src/application/cli.rs`、`src/account/**` | 共用；控制台颜色等小处按平台（`src/account/login/mod.rs:110`、`src/main.rs:1098`） | 是（M2） |
| 本地数据、日志 | `%LOCALAPPDATA%\Farwing`（`src/product.rs:189-196`） | `~/Library/Application Support/Farwing`，日志 `~/Library/Logs/Farwing` | 是 |
| 凭据 | `keyring`，默认 profile 在有后台服务时用 vault（`src/account/auth/secret.rs:20-44`） | `keyring` 的 Keychain 后端（`Cargo.lock` 已含 `apple-native-keyring-store`）；Mac 没有 vault 分支 | 是 |
| 设备中心窗口 | winit + `egui-directx11` + DirectComposition（`src/ui/windows.rs`） | winit（AppKit）+ egui 的 Metal 呈现（§4.2） | 是（M5） |
| 标题栏、菜单 | 自绘 + DWM（`src/ui/chrome.rs`） | `NSWindow` 透明标题栏、保留红绿灯；最小应用菜单 | 是 |
| 托盘、通知 | `src/ui/tray.rs`、`src/application/app/notifications.rs`（WinRT Toast） | `NSStatusItem`；`UNUserNotificationCenter` | 托盘是，通知 M9 |
| 单实例、实例间消息 | named mutex + `WM_COPYDATA`（`src/application/app/instance.rs`） | profile 目录锁文件 `flock` + Unix socket | 是 |
| 视频解码 | DXVA11（`src/platform/windows/decoder/`） | VideoToolbox `VTDecompressionSession` | H.264、HEVC 是；AV1 按探测 |
| 软件解码 | `farwing_codec` H.264、AV1（x86） | `farwing_codec` H.264（aarch64 标量）；**aarch64 没有 AV1 软件解码** | H.264 兜底 |
| 视频上屏 | 子 HWND + 独立渲染线程 + HLSL（`src/application/viewer/windows_presenter/renderer.rs`、`worker.rs`） | `CVMetalTextureCache` + Metal + 独立渲染线程 | 是（M6） |
| 键盘 | 低级键盘钩子（`src/application/viewer/windows_keyboard.rs`） | 窗口焦点内的 NSEvent，`keyCode` → HID usage | 是 |
| 拦截系统快捷键（Cmd-Tab 等） | 同一个钩子 | 可选，`CGEventTap`，需要授权【待测】 | 否 |
| 鼠标、相对模式 | raw input、`ClipCursor`（`windows_mouse.rs`） | NSEvent；相对模式 `CGAssociateMouseAndMouseCursorPosition` | 是 |
| 远端光标形状 | `windows_cursor.rs` | `NSCursor` | 是 |
| 观看窗口快捷键 | 虚拟键码存储（`src/application/viewer_shortcuts.rs:54-57`） | 改存 HID usage 并迁移 | 是 |
| 桌面声音播放 | cpal（`src/media/audio/mod.rs:772`）+ NetEq | 同一份代码，cpal 的 CoreAudio 后端 | 是（M7） |
| 麦克风送往被控 | cpal（`src/media/microphone.rs:481`） | 同上，需要麦克风授权 | 是 |
| 局域网发现、直连、WOL 唤醒 | `src/transport/direct/**`、`src/features/host/wol` | 共用；网卡与地址变化走平台层；受本地网络权限约束（§8） | M9 |
| 剪贴板、文件、拖放 | OLE、`src/features/{clipboard,file_transfer,drag_drop}` | `NSPasteboard`、`NSDragging*` | 否 |
| 聊天文字 | `src/features/chat`、`src/application/app/view/chat` | 共用（§7.3） | 是 |
| 聊天语音与通话 | 依赖 `features::host::peer::chat_video`（§7.3） | — | 否 |
| 批注、插件、HDR | `windows_presenter/plugin_*.rs` 等 | — | 否 |
| 远端升级、远程诊断、查看被控摄像头 | 按对端为 Windows 开放（`connection.rs:125`、`:348`，`device_details.rs:513-517`） | 共用，对端是 Windows 时照常可用 | 诊断、摄像头视 M9；升级是 |
| 本机作为被控的界面（被控设置、本机设备、协助、摄像头、WOL 设置） | `src/application/app/view/{host_settings,this_device,host_assist,host_cameras,wol_setup}.rs` | 不编入 | 否 |
| 安装、更新、卸载 | `src/application/app/maintenance.rs`、`platform/windows/components` | 不编入；更新查询也不编入 | 否 |

### 0.3 决定

| 事项 | 决定 |
|---|---|
| 调用系统框架 | `objc2` 系列 crate（`objc2-app-kit`、`objc2-metal`、`objc2-video-toolbox`、`objc2-core-media`、`objc2-core-video`、`objc2-io-surface`），不引入 Swift / Objective-C 源文件 |
| 平台层目录 | `src/platform/macos/`，由 `src/platform/mod.rs` 按 `target_os` 导出与 Windows 同名的模块；平台没有实现的模块不导出 |
| 被控代码 | 倾向控制面（设置、授权、状态）保留可编译，运行时（采集、编码、注入、驱动、服务）在 Mac 上不编入（§2.2 做法 B）；最终边界等 #34 回复再定 |
| 宣告 | 设备注册能力为空集合；HELLO 只声明已实现的特性；`video.recovery` 等 V3（§5.4）通过后再声明 |
| 权限验收 | 一律用固定 bundle id、Apple 签发证书签名的 `.app` 从 Finder 启动来验收（§8） |
| 最低系统与架构 | 建议 macOS 14+、仅 arm64【待确认】 |
| egui 渲染后端 | 先用能最快跑起来的后端，M8 对比后定型（§4.2） |

---

## 1. 构建与命令入口

### 1.1 Cargo 依赖

| 依赖 | 现在 | 改为 |
|---|---|---|
| `farwing-amf-sys`、`farwing-vpl-sys` | 通用（`Cargo.toml:32-34`） | `cfg(windows)` |
| `farwing-codec` | `cfg(windows)`（`Cargo.toml:91`） | 通用；`kernels/av1` 已按 `target_arch` 只在 x86 导出 decoder（`crates/codec/kernels/av1/src/lib.rs:8`），aarch64 上自然没有 |
| `egui-winit`、`winit` | `cfg(windows)`（`Cargo.toml:96-97`） | 通用；Windows 专有的 winit 扩展只在平台层使用 |
| `egui-directx11`、`farwing-nvenc-sys`、`windows`、`windows-core`、`farwing-bootstrap`、`farwing-plugin-api`、`libloading`、`memmap2` | `cfg(windows)` | 不变 |
| `objc2` 系列 | 无 | `cfg(target_os = "macos")` |
| `object` | `features = ["read_core", "std", "pe"]`（`Cargo.toml:73`） | 首版不变（插件不在范围） |

`Cargo.lock` 要随之更新；Windows 构建用 `--locked` 验证不引入新的 Windows 依赖变化。

### 1.2 `build.rs` 与 crate 根

- `build.rs:2-6`：去掉断言，`.rc` 嵌入只在 Windows 进行。
- `build_neteq`（`build.rs:141-180`）：Windows 定义 `WEBRTC_WIN` / `NOMINMAX` / `WIN32_LEAN_AND_MEAN` 并链接 `winmm`；macOS 定义 `WEBRTC_MAC` / `WEBRTC_POSIX`，按需链接 `CoreFoundation`。MSVC 专有的 `/EHsc`、`/Zc:__cplusplus` 已经是 `flag_if_supported`。评审时在 arm64 / SDK 15.5 上用 `clang -fsyntax-only` 检查过 123 个编译单元无语法错误，链接未验证【待测】。
- `src/lib.rs:3-4`：去掉 `compile_error!`；Windows 以外只允许 `macos`（其他目标保留 `compile_error!`，避免误以为支持 Linux）。
- `src/main.rs` 的 `#![windows_subsystem]` 在其他目标上被忽略，不用改。

### 1.3 命令

M1 已按代码确认 `src/main.rs` 的命令在 Mac 首版的取舍：

| 层级 | 编入 | 不编入（仅 Windows） |
|---|---|---|
| 顶层 | `Gui`、`Server`、`Login`、`Logout`、`Id`、`Devices`、`Connect`（含 `--dev-*` 参数）、`Assist`（作为主控）、`Friend`、`Grant`、`Sessions`、`Lan`、`Dev` | `WolConfigure`、`Update`、`Notification`、`Uninstall`、`Service`、`HostResident`、`InputAgent`、`CaptureAgent`、`Component`、`DisplayRecovery`、`PluginVideoHost`、`EncoderProbe`、`DesktopAccess`、`PluginHost`、`Host`、`Trusted` |
| `Dev` 子命令 | `Ports`、`Power`、`Input`、`Video` | `Plugins`、`Camera`、`Voice`（依赖被控或 Windows 后端）；`Clipboard`、`Files`、`Diagnostics`（不在首版范围内，诊断模块也有被控侧逻辑） |

上表为首版目标。M5 前，`Gui` 和默认启动设备中心只在 Windows 编入；Mac 不带子命令时打印 clap 帮助，退出码为 0（`src/main.rs:631-647`）。M6 前，`Connect`、`Assist` 只在 Windows 编入：包括 `--direct`、`--audio-only`、`--dev-*` 在内的所有路径都会打开观看窗口（`src/main.rs:902-909`、`:1098-1162`）。便携启动器 `application::bootstrap` 与 `--bootstrap-version` 也只在 Windows 编入（`src/application/mod.rs:13-14`、`src/main.rs:29-31`）；语言初始化在两边都执行（`src/main.rs:595`）。

其中，`Trusted` 整个命令不编入：它操作本机被控端的信任列表，后台服务模式下经 `transport::direct::managed` 访问 Windows 常驻服务。命令定义、分派及 `src/application/mod.rs`、`src/application/cli.rs` 中对应的 Windows 专用入口均用 `#[cfg(windows)]` 隔开。Mac 上库的编译阻断尚未清完，`cargo check` 还未走到 `main.rs` 的类型检查；本轮 Windows 构建与库测试已通过（§12）。

### 1.4 检查

两个工作流都在向开发者的 fork 推送 `feat/macos-controller-*` 分支时自动运行，不监听 PR；工作流文件进入默认分支后才能用 `workflow_dispatch` 手动运行。

- Mac：`.github/workflows/macos-check.yml` 在 arm64 runner 上用 Rust 1.95.0 执行 `cargo check --locked --target aarch64-apple-darwin`，保留失败退出码，失败时也尝试上传 `macos-arm64-check` 日志。阶段验收另做原生 `cargo build` 并运行。
- Windows：`.github/workflows/windows-check.yml` 在 `windows-2025` 上用 Rust 1.95.0 先执行 `cargo build --release --locked`，成功后执行 `cargo fast-test --locked --lib`，日志作为 `windows-x64-regression` artifact 上传。CI 替代本地 Windows 构建和单元测试回归；`docs/manual-test-checklist.md` 中对应阶段的条目仍需在 Windows 真机上验收。

## 2. 与被控代码解耦

### 2.1 现状

- `features::host` 共约 3.3 万行，其中直接引用 Windows 平台层的集中在 `peer/`、`displays/`、`input/`、`clipboard/`、`access/`、`audio.rs`、`microphone*`、`cameras.rs`、`power.rs`、`settings.rs`；`parameters.rs`、`format.rs`、`recovery.rs`、`congestion.rs`、`forwarding.rs`、`protection.rs`、`peer/screens/` 等不引用。
- `features/host/mod.rs:127` 把 `platform::{capture, encoder}` 重新导出到被控模块根。
- `access::Handle`（`src/features/host/access/mod.rs:146-161`）聚合了 WOL、电源、协助、授权、媒体缓存等，由 `AuthenticatedClient` 持有。
- 后台服务分支散在主控会经过的文件里：`resident::managed()` / `is_owner()`（`src/session/presence.rs:74`、`:489`，`src/session/device_session.rs:178-201`，`src/account/client/mod.rs:119`、`:443-445`），`vault::applies()` / `root()`（`src/account/auth/mod.rs:15-51`、`:519-520`，`src/account/server.rs:70-77`、`:185-192`，`src/transport/direct/managed.rs:292-299`），`components::maintaining()`（`src/session/presence.rs:507`、`:601`）。

### 2.2 做法

两种做法：

- A. Mac 构建整块不编入 `features::host`。约 100 处 `.host` 引用、登录流程、控制中心的状态栏和连接页都要改成“可能没有被控”，改动面大，而且把“本机是否可被控”的判断散到各处。
- B. **保留被控的控制面，运行时按平台编入**（采用）。`access`、`settings`、`assist` 的状态、`power` / `wol` 的状态与报文、`parameters`、`format` 等不依赖平台的部分照常编译；`peer`、`input`、`displays`、`audio`、`microphone`、`cameras`、`clipboard` agent 等运行时只在有后端的平台编入。Mac 上被控开关不出现在界面上，presence 不以被控身份登记，设备注册不宣告 `host`。

上游的现有参照是 Linux 主控 [PR #33](https://github.com/djkcyl/farwing/pull/33)（2026-11-28 仍为 open，未合并）：它走的是做法 A，`features::host` 整个模块和 `AuthenticatedClient.host` 字段都只在 Windows 编入。PR 未合并，不代表作者已经选定这种做法。本文倾向做法 B，边界选哪种已在 [#34](https://github.com/djkcyl/farwing/issues/34) 问题 1 中提出，回复前不动手。两种做法各能清掉多少 Mac 编译错误，目前都没有验证过。后台服务相关的分支在 Mac 上整段不编入（Mac 首版没有后台服务），不写返回 `false` 的替身函数。

### 2.3 主控用到的共享类型

M1 已将下列主控也用到的类型、常量和函数放在共用层；尚未迁出的入口仍留在被控侧：

| 用处 | 现位置 |
|---|---|
| 画质档位与码率 | `media::geometry::{Tier, tier_rate}`（主控直接引用，保留被控侧对 `Tier` 的既有重导出） |
| 视频轨道数 | `transport::rtc::tracks::TRACK_COUNT` |
| 摄像头屏幕号 | `media::camera::{DISPLAY_ID_BIT, display_id, is_camera}` |
| 共享画面帧率 | `features::stream_control::SHARED_VIEW_FPS` |
| 收听麦克风轨道 | `transport::rtc::tracks::LISTEN_TRACK` |
| HEVC 负载打包 | `media::hevc::payloads` |
| WOL 报文 | `features::wol::packet` |
| WOL 本地唤醒 | `features::host::wol::wake_locally`（待 M9 提供平台网卡后端，见 §8.2） |
| 聊天画面 | `features::host::peer::chat_video::*`（随语音与通话 `cfg(windows)`，见 §7.3） |

`features::stream_control::publisher*` 是被控一侧的发布逻辑，按做法 B 归入运行时。

## 3. 身份、能力与本地存储

### 3.1 平台身份

平台层提供唯一的“本机平台”，下列位置改用它：

- 设备注册 `src/account/auth/mod.rs:287`；
- 直连握手 `DirectHello`（`src/transport/direct/mod.rs:310`，主控和被控都发）；
- 主控直连记录 `controller_platform`（`src/session/controller/direct.rs:161`）。

被控一侧的 `lan.rs:49`、`host.rs:420`、`host.rs:600` 和服务端 `server/src/records.rs:231` 留给 Mac 被控（`2dfefe0` 的 R2），首版 Mac 不运行这些路径。

`device_init_request` 的硬件信息来自 `platform::windows::device_profile::Hardware`（`src/account/auth/mod.rs:281`、`src/account/reporting.rs:2`、`src/account/client/publication.rs:5`），改为平台层的硬件信息接口。Mac 上：机型与芯片（`sysctl hw.model`、`machdep.cpu.brand_string`）、内存（`hw.memsize`）、系统版本（`NSProcessInfo.operatingSystemVersion`）、主屏分辨率、主网卡 MAC。计算机名来自 `device_profile::computer_name`（`src/transport/direct/mod.rs:72`），Mac 上用 `SCDynamicStoreCopyComputerName` 或 `NSHost`。

M2 已完成（`510b028`）：

- 本机平台为 `platform::PLATFORM`（`src/platform/mod.rs:16-19`），上面三处主控路径改用它。
- `Hardware` 数据类型移到共用的 `src/account/hardware.rs`，字段与 serde 形状不变；各平台的 `device_profile` 提供 `Hardware::read()` 与 `computer_name()`。
- Mac 的读取在 `src/platform/macos/device_profile.rs`：
  - 计算机名用 `SCDynamicStoreCopyComputerName`；
  - `base_board` 为 `hw.model`，`cpu` 为 `machdep.cpu.brand_string`；
  - `os` 形如 `macOS 15.6 (24G84)`；
  - `screen` 为主屏原生像素（Retina 下与 Windows 的 `SM_CXSCREEN` 逻辑尺寸口径不同）；
  - `mac` 取 SCDynamicStore 主接口的 MAC，主接口没有链路层地址（如 VPN 的 `utun`）时退回 `en0`；
  - `system_uuid` 用 `gethostuuid`，取不到时记入 `errors`、不阻止注册；
  - `machine_guid`（Windows 注册表概念）与 `video` 留空。
- 本机实测：Mac16,12、Apple M4、16384 MiB、`2940x1912`，`errors` 为空。
- 设备注册的空能力集合见 §3.2；壁纸发布（`publication.rs` 的 wallpaper 与 resident 分支）以及局域网地址（§8.2）未做。

### 3.2 能力与特性

- 设备注册能力（`local_capabilities`，`src/account/feature_ability.rs`）：由已编入的后端生成。Mac 首版为空集合。服务端对空集合按 `normalize_capabilities` 处理（`server/src/http/devices.rs:255`、`server/src/db.rs:763`）。

M2 已完成：

- 注册能力与直连被控能力分开：`host_capabilities()` 是 Farwing 被控（Windows 构建）注册的集合；`local_capabilities()` 在 Windows 上等于它，其他平台为空。`FeaturePolicy::direct()`（直连到的被控）改用 `host_capabilities()`：直连对端总是 Windows 被控，与本机平台无关。否则 Mac 直连时 `Host` 判断会关掉会话里的显示设置、麦克风、拓扑等功能，`Files`（在 `stream_control` 中控制手动中转开关）也会缺失（M1 起已缺）。Windows 上两个函数的结果与改动前相同。
- 被控 HELLO 的 `touch`/`pen` 判断（`host_features`）在 Windows 以外不调用 Windows 的 pointer 接口，结果为不声明。
- 服务端实测（`server/tests/api.rs` 的 `controller_only_device_registers_without_capabilities`）：`platform: macos`、`capabilities: []` 注册成功，同一密钥刷新后设备 ID 不变、存储仍为空集合；同账号其他设备的设备列表里显示为 `macos`、`capabilities: []`；即使策略为 `control: true` 且在线，发起会话也返回 409 `host_disabled`。
- 客户端测试：`direct_policy_describes_a_windows_host`、`controller_only_builds_register_no_capabilities`（非 Windows）、`windows_registers_the_host_capabilities`（Windows）、`host_omits_touch_and_pen_without_synthetic_pointers`（非 Windows）。

待 M2 验收时在 Windows 主控上确认（按代码推断，未实测）：这台 Mac 的状态为“受限”（`src/application/app/device_status.rs:100`，`controlled_support` 为假），但下面几处只看 `platform.is_desktop()`，Mac 仍会出现：观看列表与连接页（`show_in_watching_list`，`src/application/app/mod.rs:877`）、观看窗口的切换设备列表（`src/application/viewer/device_switch.rs:106`）、本账号设备详情的电源卡片（`src/application/app/view/device_details.rs:663`）。实际操作分别被服务端（`host_disabled`）或 `src/account/power.rs:28` 拒绝。是否要在这些列表中按 `controlled_support` 过滤，验收时再定。
- 主控 HELLO 特性（`controller_features`，`src/protocol/peer.rs:244-264`）：保留 `display.config`、`display.virtual`（请求 Windows 被控改分辨率、建虚拟屏，界面与协议逻辑共用）、`cursor.participants`。M1 已在 Windows 以外去掉 `touch`、`pen`（测试 `controller_advertises_native_pointers_only_on_windows`，`src/protocol/peer.rs:394`）以及 `clipboard`、`clipboard.files`、`files`（后三项在被控 HELLO 中也已去掉）。设备注册不再宣告 `files`，其余注册能力清空留给 M2。Mac 首版的 `video.recovery` 等 §5.4 的 V3 通过后再声明，在此之前走现有的 `disable_controller_recovery`（`src/protocol/peer.rs:270-272`）同样的“只用关键帧”路径。
- Windows 以外由 `HDR_SUPPORTED` 保证不请求 HDR，解码器声明仍按探测结果发送（§5.5）。
- Mac 首版不查询更新；现有调用随设备中心只在 Windows 编入（`src/application/app/updates.rs:211`）。

### 3.3 本地数据与日志

- `product::local_data_dir`（`src/product.rs:189-196`）是 profile 目录的唯一来源；直连记录（`src/transport/direct/store.rs:30`）、Windows 的插件设置与语音设置（`src/features/voice/settings.rs:111-116`）都经它。Mac 上基准目录为 `~/Library/Application Support/Farwing`（通过 `NSFileManager` 的 `applicationSupportDirectory` 取得），profile 规则不变。
- 日志：`src/diagnostics/logging/files.rs:20` 要求 `LOCALAPPDATA`，Mac 上用 `~/Library/Logs/Farwing`。诊断包（`src/diagnostics/bundle.rs`）的系统信息部分按平台。
- `product.rs` 里的 `EXECUTABLE_NAME`、pipe 和内核对象名、`PCWSTR` 包装属于 Windows 平台层；产品名、profile、`keyring_service` 等保持共用。

M2 已完成（`b356657`）：

- `src/platform/macos/folders.rs` 经 `NSFileManager` 取 Application Support 与 Library 目录。
- 数据目录与日志目录共用 `product::profile_dir` 的 profile 规则。日志在 Mac 上与数据目录分开；Windows 仍为数据目录下的 `logs`。
- `.state` 目录在 Mac 上靠点前缀隐藏；设置文件先 `sync_all` 再 `rename` 替换；打开日志目录用 `/usr/bin/open`。
- `program_data_dir` 只在 Windows 编入。`WideName`、`wide` 与 `PCWSTR` 导入在 M2 §3.5 之后只在 Windows 编入（`011cc82`）。

遗留：

- `src/diagnostics/bundle.rs:236-247` 仍按 `local_data_dir()/logs` 找日志，`:501-509` 从日志路径的上一级推导导出目录；移植诊断包时要改。
- 共用路径里仍有写死 `LOCALAPPDATA` 的 context 文案，如 `src/account/auth/mod.rs:521`、`src/transport/direct/store.rs:30`、`src/media/preferences.rs:6`。

### 3.4 凭据

- `keyring` 4.1.6 在 `Cargo.lock` 中带有 `apple-native-keyring-store`，Mac 上走 Keychain，服务名沿用 `product::keyring_service`（如 `farwing[.p].session`）。
- `src/account/auth/secret.rs:20-44` 在 vault 适用时改用 vault。这个 vault 属于 **Windows 客户端本机的后台服务**（`src/account/auth/secret.rs:2` 引用 `platform::windows::host_service::vault`），和自建的 Farwing Server 无关，服务端不需要为 Mac 改动。Mac 没有后台服务，这个分支在 Mac 上不编入，直接走 `keyring`。这一点已在 #34 问题 1 中说明；分支怎么隔开，随 §2.2 后台服务分支的边界一起定。
- 【待测】首次访问 Keychain 是否弹出授权框，以及签名身份变化后的行为。

### 3.5 单实例与实例间消息

- `src/application/app/instance.rs`：`acquire_named` 用 named mutex，`register_window` / `window_message` 用 `WM_COPYDATA` 子类化，`activate_existing` 用 `EnumWindows` 找到已有窗口。Mac 上：profile 目录下对锁文件加 `flock`（进程退出自动释放）；第二个实例经 profile 目录下的 Unix socket 把请求交给第一个实例，再由第一个实例调用 `NSApplication.activate`。
- `src/session/presence.rs:143-154` 的 named mutex 同样改为 `flock`。
- `src/media/decoder/software_slot.rs` 的约定是同一登录会话、同一 profile 的**所有进程**共用一个软件解码槽（`:1-2`），Mac 上用 profile 目录下的锁文件 `flock` 实现，保持跨进程排他。

M2 已完成 presence 与软件解码槽（`011cc82`）：

- 进程内占用标记、线程绑定、重试与取消留在共用层；平台层只提供“试占用”。Windows 的 named mutex 原样移到 `src/platform/windows/locks.rs`。
- Mac 用 std `File::try_lock`（即 `flock(LOCK_EX | LOCK_NB)`），锁文件为数据目录下的 `locks/software-video-playback.v1.lock` 和 `locks/presence.<client_id>.lock`。锁文件不删除，进程退出由内核释放。
- presence key 先按 UUID 解析再拼进路径。Mac 的 client id 存在该用户、该 profile 的 Keychain 条目中，所以按用户隔离的锁与 Windows `Global\` 的效果相同。
- 单实例（`instance.rs`）仍在 M5。

### 3.6 系统语言与字体

- `src/i18n/mod.rs:113` 调用 `GetUserDefaultUILanguage`；Mac 上取 `NSLocale.preferredLanguages` 的第一项。M2 已完成（`ec1d60f`）：主语言子标签为 `zh` 的都按简体中文，其余（含取不到）按英文，与 Windows 对所有中文 LANGID 的处理一致；映射函数 `language_for_system_tag` 有单元测试。`src/i18n/system.rs` 中只属于 Windows 被控的文案在 Mac 构建里不使用，首版不拆分该文件，只保证不引用。
- `src/ui/fonts.rs:67-71` 从 `%WINDIR%\Fonts` 读 `msyh.ttc` 等；Mac 上读 `/System/Library/Fonts/PingFang.ttc`（或经 CoreText 按名称查找）【待测：egui 对 `.ttc` 中所需字形的加载】。

## 4. 窗口与界面

### 4.1 现状

- 一个进程一个 winit 事件循环：`src/ui/windows.rs:25-26`（`EventLoop::<Event>::with_user_event()`，加 `with_msg_hook(desktop_input_message)`）；入口 `ui::run`（`src/ui/mod.rs:22`），控制中心从 `application::app::run`（`src/application/app/mod.rs:52`）进入。
- 界面与平台无关的部分：`ui::app` 的 `App` trait、`ui::window_manager` 的 `Request` 调度（`src/ui/window_manager.rs`，观看窗口经 `viewer()` 打开，`:55`）、`application/app/view/**`、`ui/controls*`、`ui/theme.rs`。
- 与 Windows 绑定的部分：每个窗口的 `UiPresenter`（D3D11 交换链 + DirectComposition）、`create_device`（`src/ui/windows.rs:729`）、`ui/chrome.rs` 的 DWM 与 `WM_NCCALCSIZE`、`ui/tray.rs`、`ui/video_textures.rs`。egui 输出经 `egui_directx11::split_output`（`vendor/egui-directx11/src/lib.rs:83`）拆成 `RendererOutput`（`:69`）交给呈现器。

### 4.2 Mac 上的做法

M5 前，设备中心界面与窗口宿主（`application::app`、`ui::run` 等）用 `#[cfg(windows)]` 隔开；`ui::{controls, theme, branding}` 仍共用（`src/application/mod.rs:3-5`、`src/ui/mod.rs:4-35`）。

- 事件循环、窗口与输入事件：winit 的 AppKit 后端；`with_msg_hook` 一类 Windows 扩展只在平台层出现。`ui/windows.rs` 拆成通用的多窗口宿主和平台的窗口工厂与呈现器。
- egui 呈现：保持 `RendererOutput` 这一边界，平台呈现器负责把 egui 网格画进窗口。首版先用能最快跑起来的方案（现成的 egui 后端，或参照 `vendor/egui-directx11` 写一个 Metal 版），在 M8 用数据定型，比较项见 §5.6。
- 标题栏：`NSWindow` 设 `titlebarAppearsTransparent` 与 `fullSizeContentView`，保留系统的红绿灯按钮，内容区沿用现有的 egui 标题栏（拖动区域交给 `performWindowDrag`）。
- 应用菜单：关于、设置、隐藏、退出，以及编辑菜单（保证 Cmd-C / Cmd-V 在文本框中可用）。
- 托盘：`NSStatusItem` + `NSMenu`，菜单项与 `ui/tray.rs` 一致；`TrayState`、`full_exit` 的语义不变。
- 文件对话框：`NSOpenPanel` / `NSSavePanel`（可用 `rfd`，它直接调用这两个 API），替代 `platform/windows/file_dialog.rs`。首版只在导出诊断等少数地方用到。
- 不编入的界面：§0.2 中“本机作为被控的界面”和“安装、更新、卸载”。
- 首次显示：现在窗口先隐藏，首帧呈现后再显示（`src/ui/windows.rs:133`、`:210`、`:379`）。`CAMetalLayer` 不给不可见的窗口 drawable，首帧永远等不到，所以 Mac 上窗口要在创建时可见，观看窗口同理。上游 `macos-viewer` 分支的 `bbeaa01`、`b9e6012` 遇到并处理了这个问题。

## 5. 视频：接收、解码、呈现

### 5.1 现状

```
transport/rtc/receive.rs → transport/video_receiver/*
  → application/viewer/decode_pipeline.rs（解码线程、恢复、追帧）
  → media::decoder_pool::DecoderPool → media::decoder::NativeVideoDecoder
  → platform::decoder（DXVA11 或 farwing_codec）
  → DecodedFrame.surface: D3D11Surface（共享 NT handle）
  → application/viewer/present_queue.rs → windows_presenter/{worker,renderer}.rs（独立渲染线程）
```

- `present_queue.rs`、`decode_pipeline.rs`、`windows_presenter/{player,connection,chrome,fullscreen}.rs` 基本是 winit + egui + 共用逻辑；D3D 集中在 `windows_presenter/{renderer,worker,screen_windows,mod}.rs`、`windows_ui.rs` 和 `platform/windows/{surface,swapchain,video_shader}.rs`。
- 无窗口的解码路径：`dev video`（`src/application/dev_video.rs`、`src/application/viewer/dev_video.rs`）解码被控主屏、注入丢包并输出恢复统计（`docs/client-usage.md` 的 `dev video` 一节）。它是 Mac 上验证解码的第一个落脚点（M3）。

### 5.2 解码能力声明

- 主控在建会话时调用 `detect_native_decoder_support`（`src/media/decoder/mod.rs:559`，调用处 `src/transport/signal/mod.rs:397`），得到 `session::Decoder { codec, chroma444, bit10, width, height, hardware, av1_visible_size }`（`crates/protocol/src/session.rs:46`）列表。软件 H.264 的上限写死为 1920×1080，含 4:4:4（`src/media/decoder/mod.rs:577-590`）。
- Mac 上的硬件项按探测结果生成：对每个编码格式、位深、色度先查 `VTIsHardwareDecodeSupported`，再按目标参数实际创建一次 `VTDecompressionSession` 确认。不按机型写死（例如“M3 及以上支持 AV1”不保证所有 profile 和分辨率）。
- 软件项：H.264 保留；aarch64 上 AV1 软件解码不存在，不声明。
- 被控据此协商；能力不足时降级到 H.264 的路径是现有逻辑。

### 5.3 VideoToolbox 解码器

- 新增 `platform::macos::decoder`，实现与 `WindowsVideoDecoder` 相同的解码接口（`media::decode_api`）。
- 格式描述：H.264 用 SPS/PPS（`CMVideoFormatDescriptionCreateFromH264ParameterSets`），HEVC 用 VPS/SPS/PPS（`...FromHEVCParameterSets`），AV1 用 `av1C`。可以参考 commit `572014b` 删除前的 `src/decoder/platform/apple/videotoolbox/{format_desc,hevc_config,video}.rs`（`git show 572014b^:<路径>`），那时的架构不同，只借鉴参数集处理。
- 输出：要求 IOSurface 支撑的 `CVPixelBuffer`（`kCVPixelBufferIOSurfacePropertiesKey`），8 位用 `420v` 双平面，10 位用对应的双平面 10 位格式；参数集变化时重建 session。
- 失败与回退沿用 `DecoderPool` 的候选顺序：硬件失败时换软件 H.264；软件也不支持时报不支持。

### 5.4 共用层改动（A1–A3）

- A1：`VideoDecoderConfig.gpu_device` 改为平台层定义的设备句柄类型（Mac 上为 `MTLDevice` 的包装或不需要）。
- A2：`DecodedFrame.surface` 改为 `platform::surface::Surface`（Windows 上即现有的 `D3D11Surface`，Mac 上是持有 `CVPixelBuffer` 的类型）；`DecoderCandidate` 的硬件项按平台枚举；`decoder_pool.rs:34` 的 writer、`media/decoder/diagnostics.rs` 的 D3D11 探测移入平台层。
- A3：软件解码槽见 §3.5。

**V3【待测】恢复帧：** Windows 被控对声明 `video.recovery` 的主控会发长期参考恢复帧，H.264 下会出现 `frame_num` 跳号（`docs/protocol/rfi-ltr-brief.md`）。在 Mac 端用 `dev video --loss` 注入丢包，看 VT 解码器能否在恢复帧后输出正确画面：

- 能：声明 `video.recovery`；
- 不能：H.264 暂不声明（被控按关键帧恢复），或恢复期间切到软件 H.264，按实测再定。

### 5.5 上屏

- 每个视频窗口一个渲染线程，沿用 `present_queue.rs` 的排队与追帧策略（`docs/client-usage.md` 中的 `FARWING_VIEWER_CATCHUP` 说明）。
- `CVPixelBuffer` 经 `CVMetalTextureCache` 得到各平面的 `MTLTexture`，用 Metal 着色器做 YCbCr → RGB（色彩空间与范围取自 `media::video_color`，对应现在 `platform/windows/video_shader.rs` 的选择逻辑）。
- 使用期间持有 `CVMetalTexture`，在命令缓冲完成回调里释放；限制在途帧数，确认缓冲区不会在 GPU 用完前被解码器复用（V5【待测】）。
- 视频层放在 UI 层之下，用 `CAMetalLayer` 叠放，代替现在的子 HWND；`title_bar_height_pixels` 等区域计算沿用。
- 首版只做 SDR；HDR（EDR、PQ）后续另做。`HDR_SUPPORTED = cfg!(windows)`（`src/features/stream_control/mod.rs:56`）限制流请求，不过滤解码探测结果（`src/session/negotiation.rs:99`）；10 位 SDR 仍可协商（被控用 `bit10` 匹配所有 10 位格式，`src/features/host/format.rs:76`）。
- 多屏窗口：`windows_presenter/screen_windows.rs` 的“每块远端屏一个窗口”模型照搬，呈现器换成 Metal。

### 5.6 渲染后端对比（V1，M8）

通话画面和小窗经 egui 纹理进入多个窗口（`src/ui/video_textures.rs:1-4`），所以只看一个独立视频窗口不够。同一份素材，比较：

1. 现成 egui 后端 + 独立的原生视频层；
2. 自写 egui Metal 后端（参照 `vendor/egui-directx11`）+ 同一视频层；
3. 视频层分别用直接 Metal 和 `AVSampleBufferDisplayLayer` 作对照。

上游 `macos-viewer` 分支（建在 OpenUUYC 与 Linux 移植之上，不是 `next`）的 `src/platform/macos/graphics.rs`、`video_layer.rs` 是候选 1 的一个已跑通样本：egui-wgpu 跑在 Metal 上，每个窗口一个 surface；视频层支持 NV12 / RGBA、letterbox 和 90° 旋转，但走 CPU 上传。借鉴时在 commit 中注明出处。

场景：独立播放、叠加菜单与 HUD、多窗口打开关闭、全屏切换。记录显示延迟、`render_interval_p95_ms`、丢帧、CPU 是否回读像素。现成后端能否零拷贝导入 IOSurface（例如经 wgpu 的 hal 层）【待测】。

## 6. 输入

M6 前，观看窗口的呈现、Windows 输入模块与快捷键只在 Windows 编入（`src/application/viewer/mod.rs:37-62`、`src/application/mod.rs:16-17`）；主控连接、解码管线与呈现队列，以及无窗口的 `dev video`、`dev input` 仍编入。

### 6.1 现状

- 共用的状态机：`src/features/remote_input.rs`。鼠标模式 `MouseMode { View, Smart, Local, Remote }`（`:56-62`，默认智能，`src/features/viewing_settings.rs:83`）；按键入口 `key(owner, usage, down)`（`:457`），锁定键 `sync_locks`（`:497`），系统命令 `system_command`（`:429`）；按住的键集合、统一释放、心跳与超时都在这里。
- 线上按键是 HID usage（`docs/protocol/peer-messages.md:44`）。Windows 主控按 set-1 扫描码查表得到 usage，表在 `src/platform/windows/input/keyboard.rs`（`usage()` / `scan()` 在 `:171-180`，覆盖 ISO、JIS、LANG1/2 等）。
- 平台部分：`windows_keyboard.rs`（低级钩子，窗口在前台时安装）、`windows_mouse.rs`（raw input、限制光标）、`windows_cursor.rs`（光标形状）、`windows_drag.rs`。

### 6.2 HID 表移到共用层

- 把“HID usage 是否合法、是否修饰键、锁定键序号”这些判断（`remote_input.rs:15` 引用的 `keyboard::{scan, lock_index, modifier, LOCKS, KEYBOARD}`）移到共用模块（如 `features::remote_input::keys`）。Windows 的扫描码 ↔ usage 表留在 Windows 平台层。
- 按键超时计时器（`remote_input.rs:309`、`:337` 的 `platform::windows::input::deadline`）改为平台层的计时接口；Mac 上用 `tokio` 计时或 `dispatch` 计时器。
- Windows 主控行为不变：同一组 usage 能发出去，同一组被拒绝。

### 6.3 Mac 键盘

- 观看窗口获得焦点时，从 NSEvent（经 winit 或直接取 `keyCode`）得到物理键，按 Mac 虚拟键码（`kVK_*`）→ HID usage 表转换。表需覆盖 ANSI / ISO / JIS 布局，JIS 的英数、かな对应 LANG2 / LANG1。
- 修饰键来自 `flagsChanged`，左右分开（设备相关的标志位）；Cmd 默认按物理键发 Win，提供“Cmd 映射为 Ctrl”的选项，与网页端一致（`docs/protocol/web-control-brief.md` §8）。
- CapsLock：Mac 只在状态切换时报一次 `flagsChanged`，按网页端的做法把每次切换当作一次完整的按下和抬起；macOS 没有 NumLock，不发送 NumLock 的 `LockState`。
- 失去焦点、窗口隐藏、断线时统一释放按键（共用状态机已有）。
- 输入法：焦点在视频区域时不经过 `NSTextInputClient`，按物理键转发。
- 拦截 Cmd-Tab、Cmd-Space、Mission Control 等系统快捷键需要 `CGEventTap`，首版不做。所需权限（输入监控还是辅助功能）与可行性按 V6【待测】：在不同授权组合下分别用只监听和可修改两种 tap，看能否吞掉 Cmd-Tab。
- Mac → Windows 的系统命令菜单（Ctrl+Alt+Del、锁定等）照常可用：判断的是对端平台（`remote_input.rs:424-434`）。

### 6.4 鼠标与光标

- 绝对模式：窗口坐标按视频区域映射，沿用现有几何。
- 相对模式：`CGAssociateMouseAndMouseCursorPosition(false)` 加隐藏光标，增量取 NSEvent 的 `deltaX` / `deltaY`；退出时恢复关联并把光标放回窗口内。
- 滚轮：触控板的精确增量（`hasPreciseScrollingDeltas`）换算成被控的滚轮单位，尊重“自然滚动”方向（`isDirectionInvertedFromDevice`），换算方式与网页端的 1/120 档位一致（`server/web/src/lib/input.ts` 的滚轮处理）。
- 按键：左、右、中及侧键。
- 远端光标形状：被控发来的位图转成 `NSCursor`，隐藏时用透明光标。

### 6.5 观看窗口快捷键

`viewer_shortcuts::Binding.key: u16` 是 Win32 虚拟键码，已写入用户配置（`src/application/viewer_shortcuts.rs:54-57`），显示名用 `GetKeyNameTextW`（`:60-62`）。改为存 HID usage：

- 读旧配置时把虚拟键码迁移成 usage（Windows 上一次性完成）；
- 键名由平台层提供（Mac 上用 `UCKeyTranslate` 取当前布局下的字符，修饰键用 ⌘⌥⌃⇧ 符号）；
- 快捷键监听（`viewer_shortcuts` 的 watcher 和 named mutex）按平台实现。

## 7. 声音与麦克风

### 7.1 桌面声音播放

`src/media/audio/mod.rs:772` 用 `cpal::default_host()`，NetEq、Opus、SpeexDSP 都是共用代码，Mac 上走 cpal 的 CoreAudio 后端，只需 NetEq 能在 Mac 上编译链接（§1.2）。验收：音画同步、切换输出设备、拔插耳机。

### 7.2 麦克风

- `src/media/microphone.rs:481` 同样用 cpal。Mac 上首次打开麦克风会触发系统授权，`Info.plist` 需要 `NSMicrophoneUsageDescription`。拒绝时只禁用麦克风，观看照常。
- 聊天语音的 `cpal::HostId::Wasapi`（`src/features/voice/engine.rs:164`）不在本节的麦克风路径上，随聊天语音只在 Windows 编入（§7.3）。

### 7.3 聊天语音与通话

M1 已将 `features::voice` 运行时、`features::chat` 的通话分支（含 `voice_link`）及依赖 D3D11 的 `features::host::peer::chat_video` 限在 Windows（`src/features/voice/mod.rs:3-16`、`src/features/chat/mod.rs:9-10`、`src/features/host/peer/mod.rs:1169-1170`）。聊天文字与语音设置的数据格式（`VoiceSettings`、`Mode`、`Key`、`Noise`、`SoundGroup`）仍共用；`voice-settings.json` 只在 Windows 读写（`src/features/voice/settings.rs:111-151`）。设备注册与 HELLO 原本不宣告语音，无需改动。

Mac 收到 Windows 对端的 `VOICE_STATE` / `CALL_*` 推送时走现有的 `_ => {}`（`src/features/chat/state.rs:522`）；来电只在呼叫方响铃，无人接听，未接来电仍通过共用的消息预览显示（`src/features/chat/state.rs:547-551`）。

## 8. 网络与权限

### 8.1 本地网络权限

依据 [TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)：

- macOS 15 起，访问本地网络需要用户授权。所有组播地址和 255.255.255.255 都算本地网络地址；检查发生在网络栈深处，BSD socket 同样受约束。
- `launchd` daemon、以 root 运行的程序，以及 “Command-line tools run from Terminal or over SSH” 自动放行。**从 Terminal 测试看不到被拒绝的情况。**
- 本地网络权限按代码签名识别程序，ad-hoc 签名不可靠；15.0 有较多相关问题，用 15.1 及以上测试。

对主控的影响：

- 局域网发现（`src/transport/direct/lan.rs` 的组播与广播查询）、地址直连；
- 部署在局域网地址上的自建服务器：登录、信令都会受影响；
- WebRTC 连到局域网对端的 host 候选；
- WOL 魔术包（`src/account/client/mod.rs:893`）。

被拒绝时给出“本地网络访问被拒绝”的提示和去系统设置的入口，不显示成“对端离线”或“服务器不可达”。`Info.plist` 需要 `NSLocalNetworkUsageDescription`。不要假定 Wi-Fi 是 `en0`；发现报文要在所有可广播的接口上发送（现有代码按接口地址逐个发送，`lan.rs:133`、`:199`）。

### 8.2 网络信息与系统事件

主控会经过的 Windows 平台调用：

| 位置 | 用途 | Mac |
|---|---|---|
| `src/transport/direct/lan.rs:133`、`:199` `device_profile::lan_ipv4_addresses` | 本机局域网地址 | `getifaddrs`，过滤可广播接口 |
| `src/transport/signal/mod.rs:820-823`、`src/transport/nat/discovery.rs:398-399` `wol::Changes` / `Watcher` | 网络地址变化 | `SCDynamicStore` 或 `PF_ROUTE` socket |
| `src/transport/signal/mod.rs:835`、`:1042` `host_lifecycle::watch_resume` | 睡眠唤醒后重连 | `NSWorkspace` 的 did-wake 通知 |
| `src/transport/nat/route.rs` `GetBestRoute2` | 默认路由与源地址 | UDP `connect()` 探测源地址，必要时 `sysctl(NET_RT_DUMP)` |

### 8.3 `.app` 与签名

M0 的独立组装脚本 `packaging/macos/build_app.py` 接收已构建的单架构 arm64 macOS Mach-O 可执行文件，在已有的输出目录下生成 `Farwing.app`。`packaging/macos/Info.plist` 固定 bundle id `io.github.djkcyl.farwing`、`NSMicrophoneUsageDescription` 和 `NSLocalNetworkUsageDescription`。脚本要求显式提供有效的 Apple Development identity SHA-1，拒绝 `-`，不降级；在临时目录完成签名及 `codesign --verify --strict` 后才发布，不覆盖现有输出。暂不接入 `xtask`；公证、DMG、自动更新不在本文。

在有 Xcode 命令行工具（含 Python 3）的 Mac 上，从仓库根目录运行：

```sh
# 不需要私钥：检查 plist、Mach-O 平台、已有输出和签名失败路径。
/usr/bin/python3 packaging/macos/test_build_app.py

# 仅在本机用 security find-identity -v -p codesigning 查看身份，
# 将选定 Apple Development identity 的 SHA-1 存入 SIGN_ID；不要记录身份列表。
# 此测试临时编译最小 C 程序，仅返回 0，不联网、不请求权限；
# 严格验签后独立检查签名标识符和内嵌叶证书。
FARWING_TEST_SIGN_IDENTITY="$SIGN_ID" /usr/bin/python3 packaging/macos/test_build_app.py

# 单独组装：EXE 指向另行构建的可执行文件，OUT_DIR 是已有且不含 Farwing.app 的目录。
/usr/bin/python3 packaging/macos/build_app.py \
  --executable "$EXE" --output-dir "$OUT_DIR" --sign-identity "$SIGN_ID"
/usr/bin/codesign --verify --strict "$OUT_DIR/Farwing.app"
```

本机 arm64 macOS 15.6 已用临时 C 载荷通过 Apple Development 签名和严格验证；另从产物确认签名标识符为 `io.github.djkcyl.farwing`，内嵌叶证书 SHA-1 与选定身份一致。**这不是客户端构建，也不是权限通过**。Keychain 查询和签名各有 15 秒时限；失败或超时就停止，不改 Keychain、授权或签名方式。

手动待验（有实际业务载荷后，在干净用户账号的 macOS 15.1+ 上进行）：

- 从 Finder 启动开发签名的 `.app`，分别验证本地网络和麦克风首次允许、拒绝、撤销、重启后的行为。
- 本地网络判别（同一已签名 `.app`、同一 `192.168.x.x` 服务器）：
  1. 先从 Finder 启动，允许本地网络访问并确认能登录，作为对照基线。
  2. 拒绝本地网络访问（已允许时在系统设置撤销），退出后从 Finder 重新启动，验证登录是否失败。
  3. 完全退出 Finder 启动的实例，保持拒绝状态，在 Terminal 直接执行 `"$OUT_DIR/Farwing.app/Contents/MacOS/Farwing"`（不要用 `open`），重复登录并记录结果。
- 麦克风也比较两种启动方式，核对权限是否归到 Terminal【待测】。

## 9. 风险

| 风险 | 说明 | 绕过办法 |
|---|---|---|
| Windows 回归 | 共用层改动多，Mac 上验证不到 Windows 行为 | 每一步单独提交；Windows `cargo build --release`、`cargo fast-test --lib` 和对应清单条目通过后再提交 |
| `features::host` 拆分形状与上游冲突 | 做法 B 改动 `AuthenticatedClient` 与被控模块根；Linux 主控 PR #33 选的是做法 A | 已在 #34 提问，回复前不动；拆分只移动代码、不改行为 |
| VT 对恢复帧的处理 | 不行的话丢包恢复变慢 | 先不声明 `video.recovery`；按 V3 结果再定 |
| aarch64 上没有 AV1 软件解码 | 硬件不支持 AV1 的 Mac 收不了 AV1 | 如实声明，协商自动避开 |
| `farwing_h264` 在 aarch64 上只有标量路径 | 软件兜底撑不住高分辨率或高帧率 | 软件 H.264 的声明上限沿用 1920×1080；做基准（V7）后再定是否写 NEON |
| 渲染后端选择 | 自写后端工作量大，现成后端可能有拷贝 | M8 用数据决定，先跑通 |
| 本地网络与 TCC | 测试结果失真；用户首次使用门槛高 | §8 的做法；权限检查放在设置页 |
| Keychain 授权提示 | 签名变化后可能反复弹框 | 开发期固定签名身份 |
| 与上游 `macos-viewer` 分支重叠 | 上游已有人做 Mac 观看端，底座与做法不同（§5.6）；可能重复劳动，或与上游约定冲突 | #34 问题 2 已问该分支的窗口与呈现是否计划迁到 `next`；回复前先做无窗口的 M3、M4，M5 及以后等回复再推进 |
| 命令行密码输入时按 Ctrl+C | 默认的 SIGINT 处理不运行 drop guard，终端可能保持不回显 | 已在代码中注明，`stty echo` 可恢复；需要时再处理信号 |
| 与 Windows 输入行为的差异 | Cmd、CapsLock、触控板滚动的手感 | 默认行为与网页端一致，提供映射选项；双机对照测试 |

## 10. 文档与代码的出入（以代码为准）

| 文档说法 | 代码事实 |
|---|---|
| `README.md`、`docs/client-usage.md` 标题：客户端只有 Windows | 实现后补 macOS 的构建与使用说明（`client-usage.md` 增加 macOS 一节或另起一份） |
| `docs/protocol/peer-messages.md:78`：映射表在 `src/platform/windows/input/keyboard.rs` | §6.2 之后，usage 判断在共用层，Windows 扫描码表仍在该文件；同步修改此句 |
| `docs/protocol/web-control-brief.md`、`rfi-ltr-brief.md`：“原生”指 Windows 客户端主控 | 实现后“原生主控”包括 Mac；相关结论按平台复核 |
| `build.rs:4-5` 的断言信息：“other platform backends were removed” | 指 v0.3.0（`572014b`）删除的 Linux / Apple 解码器；本工作重新引入 macOS 后端 |
| `docs/dev/identity.md` | M0 已登记 macOS bundle id；M2 补了数据、日志目录与锁文件；Keychain 服务名实测与 socket 路径随 M2 凭据、M5 单实例补齐 |

## 11. 实现清单（按顺序）

每步做完：Mac 上按“验收”列确认；Windows 上 `cargo build --release`、`cargo fast-test --lib` 和相关清单条目回归；测完即提交，一步一个或几个 commit。

| 步 | 内容 | 验收 |
|---|---|---|
| M0 | 开发机安装 Rust 1.95.0 和 Xcode 命令行工具；Mac CI 在 macOS 开发分支用 Rust 1.95.0 跑 `cargo check --locked --target aarch64-apple-darwin`（编译阻断归 M1）；独立开发签名 `.app` 骨架；§8.3 的权限判别 | 骨架可组装且通过严格签名校验（已验证）；GitHub CI 实际日志、§8.3 真机权限结果待补 |
| M1 | 构建：§1 的依赖、`build.rs`、`lib.rs`、命令取舍；§2 的被控运行时按平台编入、共享类型搬出；平台层建立 `macos` 目录并只导出已实现的模块 | Mac `cargo check` 通过；Windows 构建与回归通过，行为不变 |
| M2 | 命令行登录：§3 的平台身份、空能力集合、硬件信息、数据与日志目录、Keychain；`Server`、`Login`、`Devices`、`Friend`、`Grant`、`Sessions` | 签名 `.app` 内的二进制能设置服务器、登录、列设备；Windows 主控的设备列表里这台 Mac 显示为 macOS、不可连接；登出后 Keychain 清理 |
| M3 | 无窗口解码：§5.2–§5.4（A1–A3、VT 解码器、能力探测、软件槽）；`dev video` | Mac `dev video` 连 Windows 被控：H.264、HEVC（及硬件支持时的 AV1）持续解码，统计与 Windows 主控同量级；V3 结果写回 §5.4 |
| M4 | 无窗口输入：§6.2（HID 判断移到共用层、计时器）；`dev input` | Mac `dev input` 向 Windows 被控发送脚本键鼠，被控收到的键与 Windows 主控发送的一致 |
| M5 | 设备中心窗口：§3.5–§3.6、§4（事件循环、egui 呈现、标题栏、菜单、托盘、字体、语言、单实例）；§7.3 接回文字聊天 | 从 Finder 启动，GUI 登录、设备列表、好友、授权、设置页可用；第二次启动激活第一个实例；托盘菜单可用 |
| M6 | 观看窗口：§5.5（视频层、渲染线程、多屏窗口、全屏）、§6.3–§6.5（键盘、鼠标、光标、快捷键）；断线重连 | Mac → Windows：H.264 SDR 1080p60 可看可控；左右修饰键、CapsLock、失焦与断线后按键释放；Cmd/Ctrl 映射与系统命令菜单；相对鼠标；性能门槛见下 |
| M7 | 声音与麦克风：§7.1–§7.2 | 被控声音播放、音画同步；麦克风送往被控（被控装有虚拟声卡时）；拒绝麦克风授权时观看不受影响 |
| M8 | 渲染后端对比（§5.6）并定型 | 对比数据写回 §5.6；保留一条路径，删掉另一条 |
| M9 | 扩展：4K60、HEVC、能力不足时的协商与降级、硬件 AV1；局域网发现与直连、WOL 唤醒、本地网络权限提示（§8.1–§8.2）；通知；远程诊断与查看被控摄像头 | 对应的 `docs/manual-test-checklist.md` 主控条目逐项核对；拒绝本地网络时提示正确且非局域网路径不受影响 |

性能门槛（M6、M9）：同一台 Windows 被控、同样的码流配置、显示刷新率与网络条件，对比 Windows 主控与 Mac 主控的 `native viewer session performance summary`（字段见 `src/application/viewer/mod.rs:675-706`）：`local_p95_ms`、`render_interval_p95_ms`、`dropped_present_frames`，另测丢包到恢复正确画面的时间。建议阈值：60 Hz 下 Mac 的 `local_p95_ms` 至多比 Windows 多一帧（约 16.7 ms）【待商定，不是实测结果，也不代表端到端输入延迟】。

每步完成时，在 `docs/manual-test-checklist.md` 补“macOS 主控”一节对应的条目，并在 `docs/selftest-results.md` 记录。

## 12. 状态

| 步 | 状态 |
|---|---|
| M0 | 包装脚本、测试和开发分支 CI 已配置；本机 Apple Development 签名、严格验签、标识符和证书核对通过。开发机已有 Xcode 命令行工具，Rust 1.95.0 装在仓库外的隔离目录；未实际运行 GitHub workflow，权限判别待验（§8.3）。 |
| M1 | §1.1–§1.2 构建层、§2.3 共享项迁移及 §1.3 命令取舍已完成。剪贴板、文件传输、拖放、插件、语音与通话、Windows 观看窗口及输入、设备中心和启动器均按平台编入；Mac 不声明 `touch`、`pen`，不请求 HDR。`cargo check` 仍停在 `farwing` 库，M1 结束时本机与 CI 均剩 265 个错误，Windows CI 构建与库测试通过（见 `docs/selftest-results.md`）。§2.1–§2.2 被控运行时拆分待 [#34](https://github.com/djkcyl/farwing/issues/34) 回复后定。验收条件“Mac `cargo check` 通过”要等拆分及 M2–M4、M9 中对应的阻断清完。 |
| M2 | 不依赖被控拆分的部分已完成：§3.3 数据与日志目录、§3.6 系统语言、§3.5 presence 与软件解码槽的 `flock`、§3.1 本机平台与硬件信息、命令行隐藏密码输入（`b356657`–`aa1517d`）。Windows CI 通过，Mac 检查 265 → 237，剩余分类见 `docs/selftest-results.md` 最后一节。§3.2 空能力集合与直连被控能力拆分已完成（`b53daab`；Mac 检查 237 → 236，`--tests` 239 → 237；Windows CI 构建与库测试通过）。待做：§3.4 凭据 vault 分支（Windows 本机后台服务的分支，随 #34 问题 1 的边界一起定）、Keychain 实测，以及验收所需的签名 `.app` 内命令行登录（要等库与 bin 能编译）。 |
| M3 | 进行中。A1–A2 共用解码层改用平台类型（Windows 只加别名），以及 Mac 本机显示探测（主屏物理像素，刷新率取各活动屏最高值）已提交（`64a641f`）。Mac 检查 236 → 235，`--tests` 237 → 236。Windows CI 待跑。下一步做 VideoToolbox 解码器、软件 H.264 路径和硬件能力探测。本轮验收用仓库外 harness 解码真实码流；`dev video` 连 Windows 被控和 V3 要等库能编译。 |
| M4–M9 | 未开始 |
