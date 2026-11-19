<div align="center">

# Lite RTMP Streamer

**极简 · 低耗 · 可开源的 Windows 屏幕直播推流工具**

游戏几乎不掉帧 · 预览按需开关 · 采集自动降级 · 直播绝不中断 · 不依赖 FFmpeg

![License](https://img.shields.io/badge/license-MIT-blue)
![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20x64-lightgrey)
![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust)
![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB?logo=tauri)
![Size](https://img.shields.io/badge/installer-~1.8%20MB-brightgreen)
![Build](https://github.com/QingyueBeaker/QYLive/actions/workflows/build.yml/badge.svg)

</div>

---

## 目录

- [这是什么](#这是什么)
- [合规声明（请先阅读）](#合规声明请先阅读)
- [核心特性](#核心特性)
- [快速开始](#快速开始)
- [使用流程](#使用流程)
- [架构](#架构)
- [目录结构](#目录结构)
- [参数建议](#参数建议)
- [实测数据](#实测数据)
- [常见问题](#常见问题)
- [已知限制](#已知限制)
- [开发约定](#开发约定)
- [许可与免责](#许可与免责)

---

## 这是什么

一个**通用 RTMP 推流工具**，用来替代体积庞大、常驻预览、渲染链路冗长的官方直播客户端。

它只做四件事，并且把每一件做到极致：

| 目标 | 实现方式 |
| --- | --- |
| **低游戏损耗** | 采集走 DXGI 桌面复制（GPU 内拷贝），编码走 Media Foundation MFT；无额外渲染、无合成、无常驻预览 |
| **预览 0 闲置开销** | 预览是帧调度器里的一个**动态分支**，关闭时只剩一次原子读取，不复制、不缩放、不渲染 |
| **采集 100% 兼容** | DXGI → Windows.Graphics.Capture → GDI 三级自动降级，2 秒无帧或 1.5 秒黑屏即热切换 |
| **直播不中断** | 编码/推流与采集完全解耦；切换采集源时调度器继续按固定帧率产出占位帧，RTMP 长连接不重建 |

技术栈：**Tauri 2 + TypeScript + TailwindCSS**（前端）/ **纯 Rust**（采集、编码、推流）。
不依赖 FFmpeg，不需要额外运行库，安装包约 1.8 MB。

---

## 合规声明（请先阅读）

本项目是**完全开源、非营利的通用 RTMP 推流工具**，用于学习与自用。

1. 与**哔哩哔哩（Bilibili）及其关联公司无任何从属、合作、赞助或官方关联**，不是官方产品，也未获其授权背书。
2. 仅提供**通用屏幕采集、音频采集、标准 RTMP 推流**能力，法律地位等价于 OBS 等通用直播软件。
3. **不破解、不逆向、不抓取、不自动化任何平台的私有接口**：
   - 不自动获取推流地址或串流密钥；
   - 不注入 JavaScript、不修改网页 DOM、不模拟点击；
   - 不解析接口、不抓取数据包、不请求任何私有 API；
   - 不自动登录、不保存平台账号密码；
   - 不提供弹幕、礼物、自动互动、批量开播等任何平台业务功能。
4. 所有平台账号操作、分区/标题设置、推流地址与密钥的复制，**全部由用户本人在平台官方页面手动完成**。
5. 程序只遵守公开的 **RTMP 1.0 / FLV** 规范与系统公开的 **Media Foundation / DXGI / WASAPI** 接口。
6. **纯本地运行**：不联网上报、不写云端、不收集任何使用数据（仅在推流时与你填写的 RTMP 服务器建立连接）。

“打开主播后台”按钮只是**调用系统默认浏览器打开平台官方页面**，不注入脚本、不读取页面、不拦截请求。

> 使用者应自行遵守所在平台的服务条款与当地法律法规。

---

## 核心特性

- **自研 RTMP/FLV 客户端**：纯 Rust 实现握手、chunk 流、AMF0 命令序列、ack 与断线退避重连，无第三方协议库。
- **单条永久长连接**：仅在网络真正断开时重连；开关预览、切换采集源都不会重连直播间。
- **采集热切换不断流**：切换间隙自动填充占位帧，音频全程不中断。
- **硬件编码优先，失败自动回退**：NVIDIA 显卡走 **NVENC 直连**（驱动自带的 `nvEncodeAPI64.dll`，帧以 D3D11 NV12 纹理直达编码器）；无 N 卡或驱动过旧时回退 Media Foundation，并逐个候选真跑预热帧，只有真正出码流的才被采用，最终必落到软件编码（保证一定能播）。
- **色彩转换与缩放放在 GPU**：NVENC 路径直接把采集到的 BGRA 帧交给 D3D11 Video Processor，色彩空间转换与分辨率缩放都在显存里完成，省掉每帧 3.5–7 ms 的 CPU 逐像素运算；软件编码路径保留 CPU 转换作为兜底。
- **码率控制可选 CBR / ABR / VBR**，默认 **ABR（动态平均码率）**：静态画面自动回落省带宽，动态画面逼近目标码率。
- **双轨音频 + 内置混音**：WASAPI 环回（系统内录）+ 麦克风，各自一条独立队列，按时间轴相加后统一 AAC 编码；独立音量滑块。
- **内嵌预览**：主窗口内 canvas 实时绘制，可设缩放与帧率上限，取消勾选即彻底停止。
- **零网络行为**：没有更新检查、没有遥测、没有后台进程。

---

## 快速开始

### 方式一：直接下载安装包

1. 到 [Releases](../../releases) 下载 `Lite RTMP Streamer_x.y.z_x64-setup.exe`；
2. 双击安装（或直接用免安装的 `lite-rtmp-streamer.exe`）；
3. Windows 10/11 一般已自带 WebView2；若提示缺少，安装 [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。

### 方式二：从源码构建

**环境要求**

- Windows 10 1903+ / Windows 11 x64
- [Rust](https://rustup.rs)（`stable-x86_64-pc-windows-msvc`）
- Visual Studio Build Tools（“使用 C++ 的桌面开发” + Windows SDK）
- Node.js 18+ / npm

```powershell
npm install

# 开发模式（自动起 Vite + Tauri 热重载）
npm run tauri:dev

# 打包（生成 NSIS 安装包与免安装 exe）
npm run tauri:build
```

产物位置：

| 产物 | 路径 |
| --- | --- |
| 免安装 exe | `target/release/lite-rtmp-streamer.exe` |
| NSIS 安装包 | `target/release/bundle/nsis/Lite RTMP Streamer_0.1.0_x64-setup.exe` |

> 只想用引擎：`cargo build -p lite-core`，`lite-core` 是不依赖 Tauri 的纯 Rust 库。

---

## 使用流程

1. 打开软件，点 **“打开主播后台”**（用系统默认浏览器打开平台官方页面）。
2. 在官方页面内**手动扫码登录**，设置直播间标题/分区，并**点“开始直播”**——未点开始直播时密钥不会被服务端接受。
3. 复制官方给出的 **RTMP 推流地址** 与 **串流密钥**（密钥每次打开开播设置可能会轮换，务必重新复制）。
4. 推流地址框已**预填**常见地址（`rtmp://live-push.bilivideo.com/live-bvc/`）；平台给出不同地址时以平台为准。把整条 `地址?streamname=…&key=…` 直接粘进地址框也行，程序会自动拆分。
5. 选择采集源与视频/音频参数。
6. 点 **“开始推流”**。状态灯含义：**绿色“推流中”= 服务端已接受推流**；显示“连接中 / 重连中 / 失败”说明还没推上去，按日志提示处理（多半是未开播或密钥过期）。
7. 需要看画面时勾选 **“开启预览”**（画面直接画在主窗口内，取消勾选即关闭）。
8. 结束直播点 **“停止”**，程序会干净释放所有句柄、线程与连接。

> **没有第三方开播权限（粉丝不足门槛）时**：在官方 PC 客户端里切到 **“第三方推流模式”**，它会给出本地推流地址（形如 `rtmp://127.0.0.1:1935/`）与推流码 `livehime`，把这两项填入本程序即可——最终由官方客户端完成对平台的推流，本程序只负责采集与编码。程序里也有 **“直播姬本地地址”** 按钮可一键填入。直播过程中官方客户端需保持运行。

---

## 架构

```
┌──────────────────────────────┐
│  UI (Tauri WebView, TS)      │  RTMP 地址/密钥、参数、状态、内嵌预览
└──────────────┬───────────────┘
               │ invoke / emit
┌──────────────▼───────────────┐
│  src-tauri（外壳，无业务）    │  commands + 事件转发 + 窗口管理
└──────────────┬───────────────┘
               │ lite-core
┌──────────────▼───────────────┐
│  crates/lite-core（引擎）     │
│                              │
│  capture   ← Trait 抽象      │  DXGI / WGC / GDI + 自动降级 + 热切换
│  audio     ← WASAPI + Mixer  │  系统内录 / 麦克风 / 双轨混音 / 静默补帧
│  scheduler ← 固定帧率时钟     │  主分支(编码) + 预览分支(可关)
│  encode    ← Media Foundation │  H.264 + AAC-LC
│  stream    ← 纯 Rust RTMP/FLV │  自研客户端，断网才重连
│  session   ← 装配与生命周期    │
└──────────────────────────────┘
```

**线程模型**

| 线程 | 职责 | 说明 |
| --- | --- | --- |
| `capture` | 采集 | COM/D3D 要求线程固定，独立线程持有后端，只把最新帧写入共享槽 |
| `audio` | WASAPI 采集 | 系统内录与麦克风各一个子线程，写各自的混音队列 |
| `aac` | AAC 编码 | 每 1024 采样一帧；无数据时注入静默帧，保证音频时间轴连续 |
| `scheduler` | 固定帧率时钟 | 严格按 `1/fps` 产出任务，缺帧复用上一帧，**时间戳永远均匀** |
| `encoder` | NV12 转换 + H.264 | 转换与编码同线程，避免拖慢调度时钟 |
| `stats` | 统计 | 每秒汇总码率 / 帧率 / 丢帧 |
| tokio task | RTMP 写 | 唯一异步任务，受全局限流影响最小 |

模块之间**只通过消息通道通信**（tokio mpsc / std mpsc），没有跨模块直接调用。

---

## 目录结构

```
QYLive/
├─ crates/lite-core/          # 纯 Rust 引擎（可独立作为库使用）
│  └─ src/
│     ├─ capture/             # dxgi.rs / wgc.rs / gdi.rs / manager.rs
│     ├─ audio/               # WASAPI 采集 + 双轨混音环 (mixer.rs)
│     ├─ encode/              # h264.rs / aac.rs / d3d.rs（MFT 与 D3D11 纹理路径）
│     ├─ stream/              # amf.rs / flv.rs / rtmp.rs（自研协议栈）
│     ├─ scheduler.rs         # 帧调度器 + 预览分支
│     ├─ session.rs           # 流水线装配
│     └─ config.rs / stats.rs / events.rs / convert.rs
├─ src-tauri/                 # Tauri 外壳：命令、窗口、事件
├─ src/                       # 前端（TypeScript + Tailwind）
├─ index.html / preview.html  # 主界面 / 预览入口
├─ docs/                      # ARCHITECTURE.md / COMPLIANCE.md / TESTING.md
└─ README.md
```

---

## 参数建议

| 场景 | 分辨率 / 帧率 | 码率 | 关键帧 |
| --- | --- | --- | --- |
| 竞技类游戏（高动态） | 1920×1080 @ 60 | 6000–8000 kbps | 2 秒 |
| 日常 / 聊天 | 1920×1080 @ 30 | 3000–5000 kbps | 2 秒 |
| 2K / 4K | 2560×1440 以上 | 12000–20000 kbps | 2 秒 |

上行带宽按 `码率 × 1.2` 估算（留 20% 给音频与协议开销）。

**码率控制模式怎么选**

- **ABR（默认）**：按目标平均码率走，静态画面自动掉到 1–2 Mbps，动起来逼近目标。适合绝大多数场景。
- **CBR**：恒定码率，网络与平台侧最稳，但静态画面也照样吃满带宽。
- **VBR**：质量优先，峰值不可控，部分平台对峰值敏感，慎用。

---

## 实测数据

### 本机链路实测（1080p30，软件编码，连本机 RTMP 接收器）

| 指标 | 实测值 |
| --- | --- |
| 软件编码吞吐 | 265 fps（合成帧，内容简单，仅说明编码器不是瓶颈） |
| 编码帧率 | 稳定 28–30 fps（目标 30） |
| 色彩转换（CPU，1920×1080 BGRA→NV12） | 3.50 ms/帧（≈单核 21% @60fps） |
| 缩放+转换（CPU，2560×1440→1920×1080） | 7.05 ms/帧（≈单核 42% @60fps） |
| 色彩转换+缩放（GPU，D3D11 Video Processor） | 1080p 0.95 ms／1440p 1.43 ms，且 GPU 侧完成，不占 CPU 核 |
| 音频 | 46.5 帧/秒（1024 采样 @48kHz，时间轴连续） |
| 码率 | 目标 3000 kbps 实测 2.65 Mbps；ABR 下静态画面回落属正常 |
| DXGI 采样 | 65–85 fps（1920×1080，含拷贝开销） |
| 丢帧 | 约 2 帧/秒（编码落后时主动丢帧，不破坏时间轴） |
| 安装包体积 | 1.77 MiB |

硬件编码情况（同一台机器实测）：

| 编码器 | 结果 |
| --- | --- |
| **NVIDIA NVENC 直连**（`nvEncodeAPI64.dll`） | **可用**：会话正常打开，1080p60 合成帧 250+ fps，序列头正常；界面勾选“NVIDIA 硬件编码（NVENC）”即启用 |
| NVIDIA H.264 Encoder MF | 激活阶段失败 `0x8000FFFF`；设了 GPU 偏好后可激活，但收到 D3D11 设备管理器时崩溃（懒得做了反正没人用）|
| Intel Quick Sync (MF) | 可激活，但收到 D3D11 设备管理器时**崩溃**（懒得做了反正没人用） |
| Microsoft AVC DX12 Encode | 只接受 D3D12 纹理，当前未接入（懒得做了反正没人用） |
| H264 Encoder MF（软件） | 正常，兜底方案 |

自检脚本：

```powershell
cargo run -p lite-core --example selftest        # 采集 / 转换 / H.264 / AAC 单项验证
cargo run -p lite-core --example e2e             # 本机 RTMP 接收器 + 完整链路
cargo run -p lite-core --example nvenc_test      # NVENC 创建 / 吞吐 / 码率实测（W/H/FPS/KBPS/RC）
cargo run -p lite-core --example gpu_probe       # 枚举并激活本机全部 H.264 硬件编码器
```

---

## 常见问题

**Q：软件里显示“推流中”，但直播间看不到画面？**
A：分两种情况。① 推流成功但平台还在转码，等 10–20 秒再刷新直播间；② 平台的**开播状态**没真正建立（例如没点“开始直播”、密钥已轮换），此时服务端会在 `Publish.Start` 后立刻断开。程序状态灯显示“推流中”只代表 TCP 会话被接受，二进制目标码流是否被平台识别，仍需在直播间侧确认。

**Q：设了 8000 kbps，软件里只显示更低？**
A：默认 ABR 是**平均**码率，静态画面（挂机、看网页、菜单界面）编码器会自动降到 1–2 Mbps；游戏画面动起来才会逼近 8000。若想恒定吃满带宽，把码率控制切到 **CBR**。

**Q：麦克风没声音，系统内录正常？**
A：按顺序确认①设置 → 隐私和安全性 → 麦克风 → 允许桌面应用访问；②系统声音设置里麦克风未被静音、输入音量不为 0；③默认输入设备是你要讲话的那支麦（不是虚拟设备/已拔掉的耳机麦）。另外确认界面里“麦克风”复选框是勾上的（新版默认勾选）。程序在麦克风连续 2 秒只有静音数据时会打一条警告日志。

**Q：粉丝数不够、没有第三方开播权限怎么办？**
A：本项目不绕过平台门槛。用官方 PC 客户端的 **“第三方推流模式”**，它给出的本地地址 + `livehime` 推流码填进本程序即可（界面有“直播姬本地地址”一键填入按钮），由官方客户端替你完成对平台的推流。

**Q：硬件编码用不上？**
A：先确认界面里“NVIDIA 硬件编码（NVENC）”是勾选的（默认勾选）。NVENC 需要 NVIDIA 显卡 + 较新的驱动；没有 N 卡或驱动过旧会自动回退软件编码，状态栏“编码器”会写明实际用的是哪一个、硬件还是软件。Intel/AMD 的硬件编码走 Media Foundation 路线，在部分机器上会被驱动拒绝，项目不对此做保证。

**Q：会不会被平台判定为外挂？**
A：程序只是标准 RTMP 客户端，不读取、不修改、不自动化平台任何页面与接口，与 OBS 属于同一类工具。是否允许第三方推流由平台规则决定，请自行遵守。

---

## 已知限制

1. **硬件编码**：NVIDIA 走 NVENC 直连（可用）；Intel QSV / AMD AMF 走 Media Foundation，部分机器上驱动会在收到 D3D11 设备管理器时崩溃，故该 MF 纹理直通路径默认关闭（仅 `LITE_GPU=1` 显式开启，用于实验）。默认策略是“NVENC 优先 → Media Foundation 软件编码兜底”，任何机器都能播。状态栏“编码器”会显示实际使用的编码器与（硬件/软件）。
2. **音频降级**：WASAPI 初始化失败时会依次尝试默认端点 → 全部可用端点；若全部失败则只推视频（状态栏提示）。DirectShow 采集路径未纳入默认构建。
3. **独占全屏**：极少数独占全屏游戏下 DXGI 与 WGC 都可能取不到画面，此时自动降到 GDI；建议用“无边框窗口”获得最低开销。
4. **HDR**：当前按 SDR 处理（BGRA8 → NV12，BT.601 limited range），HDR 桌面会偏淡，属预期行为。
5. **多显示器**：每个显示器一条 DXGI 复制会话，切换显示器需先停播再开播。
6. **DRM 内容**：Netflix 等受保护内容在任何屏幕采集方式下都会黑屏，属系统保护，与本工具无关。
7. **仅 Windows**：采集与编码深度绑定 Windows API（DXGI / WGC / WASAPI / Media Foundation），暂无跨平台计划。
8. **AAC 采样率声明**：内部混音与 AAC 编码按 48 kHz 运行，部分 MFT 给出的 ASC 可能标称 44.1 kHz，若遇到个别平台音频异常可反馈。

---

## 开发约定

- 所有模块通信用通道，**禁止跨模块直接调用**。
- 采集后端必须实现 `VideoCapture` Trait：`start / stop / get_frame / is_alive / kind / describe`。
- 任何新增采集路径都要在 `CaptureBackendKind::degrade_chain()` 中登记优先级。
- 预览相关代码只能出现在调度器的预览分支内，且必须先判断 `enabled`。
- 日志只写 stderr：**禁止落盘、禁止上报**。
- 提交前请跑 `cargo check`、`npm run build` 与 `cargo run -p lite-core --example selftest`。

更多细节见 [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)、[`docs/COMPLIANCE.md`](./docs/COMPLIANCE.md)、[`docs/TESTING.md`](./docs/TESTING.md)。

---

## 许可与免责

[MIT License](./LICENSE)。

本项目按“现状（AS IS）”提供，不附带任何明示或暗示担保。使用本工具产生的账号、版权、平台规则与合规责任，由使用者自行承担。本项目不提供、也不鼓励任何绕过平台权限限制的手段。

---

<sub>English: **Lite RTMP Streamer** is a minimal, low-overhead, MIT-licensed screen streaming tool for Windows 10/11, built with Tauri 2 + Rust. It provides generic screen/audio capture and a from-scratch RTMP/FLV client — no FFmpeg, no JS injection, no scraping of platform APIs. You manually paste your own RTMP URL and stream key from the platform's official page. Not affiliated with any streaming platform.</sub>
