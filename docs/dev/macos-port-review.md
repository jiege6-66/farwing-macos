# macOS 移植规划：独立评审

评审对象：`docs/dev/macos-port.md`，代码基线为上游 [`next@735d69e`](https://github.com/djkcyl/farwing/tree/735d69e841fd9816b8c396d8c8218ce3d5d94aa6)。原稿未纳入该 commit；本次审阅版本的 SHA-256 是 `9f3fab1a250455bbfc07f5754f59d63239f7a22bc85b544237b2924a814e28ae`。下文的 A、P 编号沿用原稿，连接方向一律写作「主控 → 被控」。

**结论：保留原生媒体栈和先主控、后被控的顺序；不要等 17 项 Windows 抽象全部完成，才在 Mac 上验证第一条真实连接。** 最直接的反例：即使 Mac 已能注入按键，只要它如实上报平台，现有 Windows 主控就不会把普通按键发给它。下文区分代码事实、Apple 的公开约束和仍需真机验证的建议。

## 1. 上游要求与本评审建议的边界

- [作者在 Issue #5 的置顶回复](https://github.com/djkcyl/farwing/issues/5#issuecomment-6107018029)强调后端边界、编解码与呈现效率、输入和功能体验；也明确说「你可以先在自己的 fork 中继续迭代，再将适合上游的通用改进整理成 PR」。谈的是 Linux 移植和对未来平台的期望，不是 macOS 合并承诺，也没有指定 PR 数量或 UI 渲染器。
- [作者在 Linux PR #1 的反馈](https://github.com/djkcyl/farwing/pull/1#issuecomment-6211499564)要求注意平台之间的互联，举例就是遗漏 Mac 键位映射；[后续评论](https://github.com/djkcyl/farwing/pull/1#issuecomment-6248328462)要求 Windows 能用的改进一起做。评审时该 PR 面向 `main`，尚未合并；现版是 `next` 上的自托管 Farwing。应借鉴交叉测试的方法，不把旧客户端的兼容对象当成本版目标。
- 本仓库的[平台层约定](../../src/platform/mod.rs#L1-L9)要求产品策略留在 session/features，不放未来平台的占位实现。**下文建议的是按可运行的功能拆分，而非用空后端换取编译通过。**

## 2. 影响互通或阻塞阶段验收的问题

### R1 · 普通键盘、系统命令和本地快捷键是三件事（§4 A6、§5.3、§6、P4）

**代码事实：** [`keyboard_supported()` 和 `system_command()`](../../src/features/remote_input.rs#L419-L434) 都判断对端是不是 Windows；[`key()`](../../src/features/remote_input.rs#L455-L464) 对非 Windows 对端直接返回，按键不会入队。改 Mac 的 `CGEventPost` 或改本地快捷键存储，都不能修复这个发送端问题。反过来，如果在 Mac 构建里一律编译掉 Windows 系统命令，也会误删 Mac 主控操作 Windows 被控的能力。

**建议与验收：** 按本端角色、对端能力和会话控制权分别判断普通 HID 键、锁定键同步、Windows 系统命令。用真实上报 `macos` 的设备测试 Windows → Mac；反向测试 Mac → Windows 的物理键、可选 Cmd/Ctrl 映射和系统命令菜单。两个方向都测左右修饰键、CapsLock、窗口失焦或断线后的按键释放；不能以「把 Mac 报成 Windows 后能用」通过验收。

### R2 · 平台身份和功能声明不止一处（§2、A13–A14、P2–P4）

| 路径 | 基线行为 | 会发生什么 |
|---|---|---|
| [设备注册](../../src/account/auth/mod.rs#L280-L293)、[LAN 公告](../../src/transport/direct/lan.rs#L43-L58) | 两处都写死 `Platform::Windows` | 只改注册会让同一台 Mac 在账号设备列表和 LAN 中身份不同。 |
| [注册能力](../../src/account/feature_ability.rs#L36-L49)、[会话 HELLO](../../src/protocol/peer.rs#L244-L295) | 前者固定上报 `host/files/ports/power` 等；后者分角色，但静态集合仍包含虚拟屏、文件剪贴板等 | 只有主控的构建可能被当作可连接的被控；远端可能展示未实现的功能。只隐藏本机按钮无效。 |
| [服务端直连记录](../../server/src/records.rs#L217-L244) | 没有旧记录、且主控先上报时，被控平台以 `Platform::Windows` 写入；若被控先上报则会用其真实身份覆盖初值 | `server/` 不需要做 OS 移植，但不能排除在互通验收之外。 |
| [更新查询](../../src/account/api/mod.rs#L1046-L1049) | 请求 Windows 的更新路径 | 在 Mac 更新机制实现前应禁用该路径，而非向 Mac 推荐 Windows 包。 |

**建议与验收：** 明确区分「我是哪个平台」「这个版本实现了什么」「用户或本次会话允许什么」。P2 断言只有主控的 Mac 不宣告 `host`；P4 对照账号注册、LAN 公告、直连记录、双方 `HELLO` 和远端 UI。Mac 被控不支持虚拟屏，不代表 Mac 主控不能请求支持该功能的 Windows 被控创建虚拟屏。

### R3 · P2 缺少真实的 Cargo 和命令入口边界（A16、P1–P2）

**代码事实：** AMF/VPL 在[通用依赖](../../Cargo.toml#L32-L34)，而 `farwing-codec`、`egui-winit`、`winit` 在 [Windows target 依赖](../../Cargo.toml#L86-L98)。[application 入口](../../src/application/mod.rs#L20-L70)仍无条件引用 Windows 安装、服务和 capture agent。移除 `compile_error!`、改 `.rc` 和 NetEq 宏，并不能让 `login` / `devices` 自动成为可构建的 Mac 功能。检查源码里有没有 `windows::` 也发现不了 R1、R2。

**建议与验收：** 先整理目标依赖、模块导出和 CLI 命令注册，让同一个真实 Mac 客户端能完成登录和查看设备；没有后端的命令整条不编入。Mac CI 至少做 `cargo check --locked --target aarch64-apple-darwin`，阶段验收再做原生链接和运行；Windows 保留 release 构建及对应回归测试。让 A1–A17 随用到它们的真实功能逐批进入，而不是取消 Windows 回归要求。

### R4 · 发布包可以晚做，权限主体不能晚定（§5.5、P3–P4、P6）

P3 已经要用麦克风和通知，P4 要录屏和控制，原稿却到 P6 才组装 `.app`。固定签名的 Terminal/CLI 测试也不能替代用户从 Finder 启动实际 app 的首次授权。权限清单还漏了 [macOS 15 起的 Local Network 隐私权限](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)，它影响 LAN 发现和直连，不能笼统显示为「对端离线」。

**建议与验收：** P0/P2 就有固定 bundle ID、开发签名和用途说明的最小 `.app`；公证、DMG 和自动更新仍可留到 P6。区分普通前台窗口的键盘事件与可选的全局捕获，不为基础观看索取 Input Monitoring。在干净用户下从 Finder 测首次允许、拒绝、撤销和重启：拒绝麦克风仍可观看；允许录屏但拒绝输入控制时只能观看；拒绝 LAN 时给出对应提示，不影响可用的非 LAN 路径。

## 3. macOS 原生方案：哪些可采用，哪些仍需验证

### R5 · 统一内存不等于端到端零拷贝（§1、A2/A4、§5.1）

- **被控发送：** `SCStream → CVPixelBuffer → VTCompressionSession → 压缩码流`。**主控接收：** `压缩码流 → VTDecompressionSession → CVPixelBuffer → CVMetalTextureCache → Metal`。`IOSurface` 可作为像素缓冲的底层存储，不是编码后的输出，也不决定权限和进程归属。
- 本机 SDK 15.5 的 `SCStream.h` 在 `pixelFormat` 处列出 `420v/420f`、`xf44`、`RGhA` 等，**未把 P010 列为保证的直接输出**。这不等于所有机型都不能得到 P010。SDR 先验证 NV12；HDR 从 [Apple 的 HDR capture preset](https://developer.apple.com/videos/play/wwdc2024/10088/) 开始，记录实际格式、色彩元数据与编码器要求，仅在必要时转换。对照 [SCK 的 pixelFormat 文档](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/pixelformat)。
- 把目标写成「硬件主路径不把像素读回 CPU」，而非「全链路一定没有拷贝」。SDK 的 `CVMetalTextureCache.h` 要求使用期间持有 `CVMetalTexture`；应验证 GPU 完成前 buffer 不被复用，同时限制在途帧数。用实际路径和队列数据验收，不能只看 FPS。

### R6 · VideoToolbox 的 LTR 不是 Farwing 的两个参考槽（§5.1、P3–P4）

[Farwing 的 `FrameControl` / `LtrOutput`](../../src/media/encoding/mod.rs#L193-L208)要求指定 `mark_ltr` / `use_ltr` 槽，并报告实际编码的参考。[Apple 的 LTR 接口](https://developer.apple.com/videos/play/wwdc2021/10158/)则由编码器在输出中给 token，调用方确认 token 后请求 refresh；编码器可使用已确认的参考，无确认时可输出 IDR。**不能把请求的槽当作实际引用填进 `frame-meta`。**

首版可按[现有恢复设计](../protocol/rfi-ltr-brief.md)走 IDR 兜底，不宣称已经支持 LTR 恢复。分别验证两个方向：① Windows 生成 H.264/HEVC 恢复帧，Mac 解码时遇到缺帧/跳号，画面能否正确恢复；② Mac 编码时，两名观看者确认不同的参考，能否证明实际依赖，并在证明不了时退回 IDR。AV1 亦应先查 `VTIsHardwareDecodeSupported`，再创建与目标 profile、位深和分辨率匹配的实际 session；「M3 及以上」不是对所有格式的保证。

### R7 · 独立播放器与 egui 内嵌视频要分别比较（§5.1、P2、§9）

独立播放器既然准备把 UI 层和视频层分开，就不能仅凭「wgpu 不易导入 IOSurface」断定整个 egui 渲染器必须自写。但[通话画面确实通过 egui 纹理进入多个窗口](../../src/ui/video_textures.rs#L1-L12)，只演示一个独立视频窗口也不够。

在相同素材上比较「现成 egui 后端 + 独立原生视频层」和「自写 egui Metal 后端」，以 [AVSampleBufferDisplayLayer](https://developer.apple.com/documentation/avfoundation/avsamplebufferdisplaylayer) 或直接 Metal 呈现作对照。至少测独立播放、叠加 UI、通话缩略图、多窗口关闭/重建；记录显示延迟、丢帧和 CPU 像素回读。再决定维护哪条路径，本评审不预设胜者。

### R8 · 桌面混音与按进程采音不是同一需求（§5.2、§9）

如果通话播放和采集在同一进程，只为排除 Farwing 自己的声音，SCK 在 macOS 13 已有 [`excludesCurrentProcessAudio`](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/excludescurrentprocessaudio)。若要单独采某个进程，或排除多个 helper，再评估 [Core Audio taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps)（Apple 示例要求 14.2+ 和 `NSAudioCaptureUsageDescription`）。14.2+ 可以是产品支持策略，不是基础桌面串流本身的硬门槛。用「远端听到目标应用、听不到自己的回声」分别验证两条路径。

### R9 · 无人值守不能建立在「只有一个会话」之上（§5.4、§6、P6）

前台只有一个控制台用户，不等于只有一个已登录 GUI 会话。[`SMAppService`](https://developer.apple.com/documentation/servicemanagement/smappservice) 的 `register` 规则区分两者：LaunchAgent 按用户注册，LaunchDaemon 启动需管理员批准；daemon 有 root 身份，不等于有某个用户的录屏和输入授权。`CGSessionCopyCurrentDictionary` 返回会话信息，不应把「已登录」直接等同于「屏幕未锁定」。

首版被控只承诺**已登录、当前活动且已授权的用户桌面**。交互式共享可评估 [`SCContentSharingPicker`](https://developer.apple.com/documentation/screencapturekit/sccontentsharingpicker)；Apple 的 [Persistent Content Capture entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.persistent-content-capture) 面向 VNC 类应用，须申请，不是 Farwing 已获得的权限。分别测试切换用户、锁屏、注销、重启后尚无人登录：不能把前一位用户的画面或输入接到下一位；未支持的状态要明确停流，而不是以「服务仍在线」判作成功。

## 4. 两处会改变既有行为的细节

| 原稿 | 已确认的差异 | 修订与验收 |
|---|---|---|
| **A3：软件解码槽可改进程内锁** | [现有契约](../../src/media/decoder/software_slot.rs#L1-L17)是同一登录会话、同一 profile 的**所有进程**共用一个槽；单进程锁无法维持它。 | 保留跨进程排他。测同 profile 第二进程被拒、不同 profile 互不阻塞、持有者异常退出后可重新获取。 |
| **A15 / P5：插件改为 `.dylib`** | [插件发现](../../src/plugins/metadata.rs#L39-L51)读取 `.fwmeta`，[扫描](../../src/plugins/metadata.rs#L87-L99)只认 DLL；[视频插件](../../src/plugins/video.rs#L114-L128)只接受 `d3d11-rgba16f` 和 `DXBC`，不是只换加载器或扩展名即可。 | 分开验收普通插件的 Mach-O 元数据/签名策略和视频插件的 Metal shader 表示；未实现的后端明确拒绝。发布策略要考虑 [hardened runtime 默认的 library validation](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.disable-library-validation)。 |

## 5. 建议的验收顺序与证据边界

1. **构建与身份：** Windows 原有路径回归；签名 Mac `.app` 能登录并正确上报身份和能力。同步做一个小规模 SCK → VT 被控采集验证，提前暴露格式和授权问题，不等于提前交付完整被控。
2. **第一个可用版本：** Mac → Windows，先做 H.264 SDR 1080p60、键鼠、断线重连。随后验 4K60、HEVC、能力不足时的协商与降级，再比较渲染后端。P4 补 Windows → Mac、浏览器 → Mac；两端实现后再测 Mac → Mac。
3. **性能门槛先约定再测：** 相同 Windows 被控、相同码流配置、显示刷新率与网络条件，对比 Windows 主控和 Mac 主控。使用[现有 summary 字段](../../src/application/viewer/mod.rs#L675-L706) `local_p95_ms`、`render_interval_p95_ms`、`dropped_present_frames`，另测丢包到恢复正确画面的时间。举例：60 Hz 下 `local_p95_ms` 允许比 Windows 多至多一帧（约 16.7 ms）；**这是待商定阈值，不是实测结果，也不是端到端输入延迟。**

**已经验证的范围：** 上游作者回复、上述基线代码、Apple 公开资料及本机 SDK 头文件。另在 arm64 Mac、macOS SDK 15.5 上，按 [NetEq 清单](../../vendor/webrtc_neteq/sources.txt)加 [bridge](../../src/media/audio/neteq_bridge.cc)，保留 [build.rs 的通用宏](../../build.rs#L141-L178)，用 `WEBRTC_MAC` / `WEBRTC_POSIX` 代替 Windows 宏，以 `clang -fsyntax-only` 和 `clang++ -std=c++17 -fsyntax-only` 检查了共 123 个 C/C++ 编译单元，未见语法错误。**没有验证链接、整个 Mac 客户端构建、TCC 实际行为或串流性能。**

**请复核者优先回答：** 哪些阻塞项能由同一基线的代码反证？哪些结论只对某个 SDK、系统版本、机型或连接角色成立？对于 LTR、HDR、渲染器和无人值守，哪个最小真机用例足以改变方案选择？请把反证位置或判定条件附在对应 R 编号下。
