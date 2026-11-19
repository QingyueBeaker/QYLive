# 架构说明

## 1. 分层

```
UI (TS/HTML/Tailwind)
   ↓ invoke / listen
src-tauri（Tauri 命令层，零业务逻辑）
   ↓ 直接调用 lite-core 的公开 API
lite-core（引擎库，可独立复用）
```

`lite-core` 不依赖 Tauri，可被任何 Rust 程序作为库引用（例如做成 CLI 或嵌入其他工具）。

## 2. 引擎模块与对外接口

| 模块 | 关键类型 | 职责 |
| --- | --- | --- |
| `config` | `RunConfig` / `VideoConfig` / `AudioConfig` / `SourceConfig` | 纯数据配置，可序列化 |
| `capture` | `VideoCapture` trait、`VideoFrame` | 采集后端抽象与帧结构 |
| `capture::manager` | `spawn_capture`、`SharedFrame` | 自动降级、热切换、最新帧共享槽 |
| `audio` | `AudioVolumes`、`spawn_audio` | WASAPI 采集、gain 实时调整 |
| `audio::mixer` | `Mixer` | 48k/双声道 f32 混音环，按 1024 采样出块 |
| `encode::h264` | `MfH264Encoder` | Media Foundation H.264，候选预热 + 硬件优先 + 软件兜底 |
| `encode::aac` | `MfAacEncoder` | Media Foundation AAC-LC |
| `stream::flv` | `FlvTag` | FLV tag 构造、Annex-B ↔ AVCC |
| `stream::rtmp` | `publish_loop` | 纯 Rust RTMP 客户端 + 断线重连 |
| `scheduler` | `SchedulerArgs`、`PreviewState` | 固定帧率时钟与预览分支 |
| `session` | `start()`、`StreamSession` | 流水线装配与生命周期 |
| `stats` / `events` | `Stats`、`EngineEvent` | 指标与 UI 事件 |

## 3. 数据流

```
DXGI/WGC/GDI ──frame──► SharedFrame(Arc<Mutex<Option<Arc<VideoFrame>>>>)
                                   │
                        scheduler（1/fps 时钟）
                          ├─ 主分支：frame → 编码线程 → BGRA→NV12 → H.264 → FLV → RTMP
                          └─ 预览分支（enabled=false 时整段跳过）
                                   │
                             PreviewState(Mutex<Vec<u8>> + seq)
                                   │ invoke("preview_frame") 二进制返回
                             预览窗口 Canvas.putImageData

WASAPI loopback ─┐
                 ├─► Mixer(48k stereo f32) ─► AAC 线程（1024 采样/帧）─► FLV ─► RTMP
WASAPI mic ──────┘
```

## 4. 为什么能“不断流地热切换”

RTMP 的时间轴由 **scheduler 独占控制**：它按 `tick * 10_000_000 / fps` 生成视频 PTS，与采集是否成功无关。

- 采集线程取不到帧 → 共享槽里仍是上一帧，调度器继续出帧（编码线程复用上次 NV12）；
- 采集后端切换 → 只影响“下一秒共享槽里写什么”，不影响时间戳序列；
- 编码线程积压 → 丢弃视频帧并计数，绝不阻塞时钟。

因此 RTMP 消息流的时间戳始终均匀递增，服务端与观众端都不会断播。

## 5. 编码器选择策略（实测驱动）

1. `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER)` 枚举所有能接收 NV12、输出 H.264 的 MFT，按“硬件优先、软件兜底”排序。
2. 对每个候选：`MF_TRANSFORM_ASYNC_UNLOCK`（硬件多为异步 MFT）→ `MF_LOW_LATENCY=1`（否则编码器会缓冲全部帧）→ 设输出类型 → 设输入类型 → 启动流。
3. **预热探测**：真跑 6 帧黑帧，能产出码流的候选才被采纳；否则换下一个（异步事件、流类型变更、ProcessOutput 语义都已按协议实现）。
4. 序列头优先取 `MF_MT_MPEG_SEQUENCE_HEADER`；取不到时用首帧 SPS/PPS 兜底构造 `AVCDecoderConfigurationRecord`。

已知：QSV / NVENC 的 MF 编码 MFT 多数只接受 **D3D11 纹理**，系统内存 NV12 会被拒绝，因此本机自动落到软件 MFT。零拷贝 D3D11 输入路径属于后续优化项。

## 6. RTMP 实现范围

实现：简单握手、SetChunkSize、connect / createStream / publish、Acknowledgement、FLV tag 分块写入、序列头缓存与重连补发。

不实现：服务端命令语义解析（只做错误关键字扫描）、加密 RTMPS、H.265/AV1。

## 7. 预览的“零开销”边界

关闭预览时，调度器每帧只执行一次 `AtomicBool::load(Relaxed)`。`PreviewState::set_enabled(false)` 会立即清空并 `shrink_to_fit` 缓冲区，避免内存驻留。前端在 `document.hidden` 时停止 IPC 拉取，因此预览窗口最小化也不会持续渲染。
