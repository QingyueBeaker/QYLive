# 验证记录

本文件记录本项目在真机（Windows 11 x64）上的实际验证结果，避免“只编译不运行”。

## 1. 自检（`cargo run -p lite-core --example selftest`）

| 项目 | 结果 |
| --- | --- |
| DXGI 桌面复制 | 启动成功，1920×1080、stride 7680，采样 65–85 fps |
| 首帧 | 启动瞬间可能为全黑（Desktop Duplication 尚未取到桌面画面），属正常 |
| BGRA→NV12 | 1382400 字节（1280×720×3/2，符合预期） |
| H.264 软件编码器 | 1280×720，60 帧耗时约 74 ms（≈800 fps），avcC 38 字节 |
| H.264 硬件编码器 | QSV/NVENC 的 MF 编码 MFT 需要 D3D11 纹理输入，预热即被跳过（见 README 限制 #1） |
| AAC-LC | ASC `[0x12,0x10]`，100 帧（约 2.1 s 音频）耗时 14 ms |
| RTMP URL 解析 | `rtmp://host/live-bvc/?x` → `host:1935` + `live-bvc` |

## 2. 端到端联调（`cargo run -p lite-core --example e2e`）

本机起一个只做 RTMP 握手的最小接收器（`127.0.0.1:11935`），跑完整流水线 8 秒：

| 指标 | 实测 |
| --- | --- |
| 视频 | 226 帧 / 8 s ≈ 28 fps（目标 30），编码帧率 29.9 |
| 音频 | 372 帧 / 8 s = 46.5 帧/秒（1024 采样 @48 kHz，时间轴连续） |
| 码率 | 2.65 Mbps（目标 3 Mbps CBR） |
| 发送/接收 | 发送 2507 KB，对端收到 2509 KB（TCP 已确认一致） |
| 丢帧 | 16 帧（编码线程落后时的主动丢帧，不影响时间戳均匀性） |
| 事件 | `AudioStatus` / `SourceChanged` / `EncoderInfo` / `Stats` 均按预期上报 |

判定：**通过**（有真实字节落到对端，音视频时间轴连续）。

## 3. 修复记录（真实环境暴露的问题）

| 现象 | 根因 | 处理 |
| --- | --- | --- |
| `SetInputType` 返回 `0xC00D6D77` | 硬件 MFT 是异步 MFT，未解锁 | 设 `MF_TRANSFORM_ASYNC_UNLOCK` |
| `ProcessOutput` 返回 `0xC00D6D60` | 编码器要求先设输出类型再设输入类型 | 调整设置顺序 |
| 编码器只吐空数据、`avcC` 长度 0 | 编码器内部缓冲，需 DRAIN 才出帧 | 设 `MF_LOW_LATENCY=1` |
| `ProcessOutput` 返回 `E_UNEXPECTED` | 异步 MFT 未按事件驱动取输出 | 只在 `METransformHaveOutput` 后调用 `ProcessOutput` |
| 首帧后输出类型变更导致永久停摆 | 未处理 `MF_E_TRANSFORM_STREAM_CHANGE` | 重新协商输出类型并刷新序列头 |
| 部分硬件 MFT 接受类型但永不产出 | 只支持 D3D11 纹理输入 | 增加 6 帧预热探测，失败自动换候选 |
| 直播只有 3 fps | 调度器帧间隔写成 `10^10 ns / fps` | 修正为 `10^9 ns / fps` |
| 音频时间轴被拉长 | 静默补帧按循环次数而非墙上时间 | 改为按真实时间补帧（46.875 帧/秒） |
| `SourceChanged` 每 500 ms 刷一次 | 心跳事件 | 仅在采集源真正变化时上报 |
| 点“开始推流”后整个程序闪退 | `start_stream` 是同步命令，线程里没有 tokio 运行时，`session::start` 内部 `tokio::spawn` panic，release 下 `panic=abort` 直接退出进程 | 改为 `tauri::async_runtime::block_on(async { lite_core::start(..) })`，并去掉 `block_in_place`（避免 current_thread 运行时 panic） |
| 真实平台推流每 1–2 秒断连重连 | 经 ffmpeg 对照实验确认为**服务端会话级拒绝**（详见第 4 节），非客户端协议错误 | 1) 补发 FMLE 标准 `releaseStream` / `FCPublish`；2) chunk 4096 → 1360；3) 编码器强制 baseline(66) + `CODECAPI_AVEncMPVDefaultBPictureCount=0`；4) 重连改为 2/4/8/15 秒退避，避免秒级硬连触发风控 |
| 点“打开主播后台”后窗口空白/不出现 | Tauri 的窗口必须在主线程创建；命令与后台线程直接 `WebviewWindowBuilder::build` 得到不可见窗口 | 全部改为 `app.run_on_main_thread()` 内建窗（预览窗口同样修正）；补桌面 UA 与 `livepanel` 窗口能力；创建失败自动退回系统浏览器 |
| 地址框粘贴整条 `地址?streamname=…` 时密钥丢失 | `split_url` 会丢弃 URL query | 前端自动把 `?` 之后的内容切到密钥框，并补齐末尾 `/` |
| release 版仍打不开主播后台；主播后台/预览窗口关不掉、界面无响应 | 内嵌 WebView 第二窗口在部分机器上创建时阻塞主事件循环（窗口无消息处理 → 无法关闭/无响应） | 彻底移除多 WebView 窗口方案：主播后台改为系统默认浏览器打开；预览改为主窗口内嵌 canvas 渲染，命令侧只切引擎开关 |
| 地址框留空点“开始推流” → 日志报“RTMP 地址必须以 rtmp:// 开头”，看起来像按钮没反应 | `readConfig()` 对空串无条件补尾斜杠，空地址被补成 `"/"`，绕过前端“地址为空”校验直达后端 | 只有地址非空才补 `/`；新增 `rtmps?://` 前缀前端校验；所有校验/失败同时写状态栏 `#statHint` 红字 |
| 引擎在跑（重连中）时“开始推流”按钮被置灰，点不动 | `setRunning(true)` 直接 `btnStart.disabled = on` | 改为只有“启动中/已 publishing”才禁用；重连中/失败时按钮显示“重试连接”可再次点击，后端 `start_stream` 会先 `stop()` 旧会话再重启 |
| 状态栏显示“推流中”但手机端看不到直播 | 原来 `start_stream` 成功即视为“推流中”，而它只代表采集/编码起来了；首次连接失败还会静默结束任务 | 新增 `RtmpStatus` 事件（connecting/publishing/retrying/failed/stopped），状态灯只在服务端 `Publish.Start` 后变绿；首次连接失败改为退避重试并上报原因 |
| 填直播姬本地入口 `rtmp://127.0.0.1:1935/` 点开始 → “开播失败：RTMP 地址缺少 app 路径” | `split_url` 强制要求 `/` 之后有 app 段 | 允许 app 为空（无 `/` 或 `/` 后为空都合法）；本地转发服务器（直播姬第三方推流入口）就是这种形式 |
| 设 8000 kbps 实际只跑约 2400 | 只设了 `CODECAPI_AVEncCommonRateControlMode=CBR`，**没设目标码率** `CODECAPI_AVEncCommonMeanBitRate`；输出类型里的 `MF_MT_AVG_BITRATE` 多数 MFT（尤其硬件）不认 | 按 CBR/ABR/VBR 分别设置 Mean/MaxBitRate；新增 ABR（PeakConstrainedVBR，均值=目标、峰值=2 倍） |
| 硬件编码器全部被淘汰、只能用软件编码 | ① QSV 需要 D3D11 纹理输入，系统内存 NV12 被拒（“编码器持续拒绝输入”）；② NVIDIA MFT 激活 0x8000FFFF；③ Microsoft AVC DX12 Encode 只吃 D3D12 纹理（输出类型 0x80041000）；④ 预热只跑 6 帧，硬件流水线延迟常需 10+ 帧 | 预热按硬件/软件分别给 14 / 6 帧；枚举时打印全部候选与硬件标记。真正启用 GPU 仍需 D3D11 纹理直通（下一步） |
| 显示“推流中”但直播间始终无画面 | MF 的 `MF_MT_MPEG_SEQUENCE_HEADER` 是 Annex-B（多个 SPS/PPS），被当成 avcC 直接发出去，`numOfSPS` 字段是垃圾值 | 本地 mediamtx 复现：`unable to parse H264 config: sps count != 1 is unsupported`。新增 `flv::to_avcc_record()` 统一解析 SPS/PPS 并重建合法的 AVCDecoderConfigurationRecord（1 SPS + 1 PPS）。修复后 mediamtx 收流正常、ffprobe 识别为 H264 Constrained Baseline + AAC LC |

## 4. 真实平台推流（B 站）对照实验

用 3 种客户端推同一个地址与密钥，观察服务端行为（密钥不写入仓库，仅运行期通过环境变量注入）：

| 客户端 | 现象 |
| --- | --- |
| 本项目 `live_test`（完整音视频） | `NetConnection.Connect.Success` → `createStream` `_result`（stream_id=1）→ `NetStream.Publish.Start` → 服务端立刻 EOF，写失败 `os error 10053` |
| 本项目 `live_test`（仅视频 / 仅音频） | 与上一行完全相同，说明与音频、与媒体帧内容无关 |
| 本项目 `rtmp_probe`（只发命令，不发任何媒体帧） | 连接 / releaseStream / FCPublish / createStream 全部存活，**一发 `publish` 就被 EOF** |
| 便携版 ffmpeg（`-c:v libx264 -profile:v baseline -b:v 2500k`） | 约 1 秒后 `Error number -10053 occurred`（同样是服务端中止），debug 日志中没有任何 onStatus 错误 |

结论：**服务端在 `Publish.Start` 之后主动关闭会话，与客户端实现无关**（自研客户端与 ffmpeg 表现一致）。这属于密钥/开播状态侧的问题，典型原因：

1. 直播间未点“开始直播”（未开播时密钥不被接受）；
2. 串流密钥已轮换过期（每次打开开播设置都会刷新）；
3. 同一 streamname 仍有旧会话占用，短时间内重复连接会被踢；
4. 账号开播权限/实名状态异常。

排查建议：在后台点“开始直播”后重新复制一次地址与密钥，等 1–2 分钟再推；仍不行则换用官方直播姬验证同一密钥。

## 5. 应用层

| 项目 | 结果 |
| --- | --- |
| `cargo check -p lite-rtmp-streamer` | 通过 |
| `npm run build`（Tailwind + Vite） | 通过，产物在 `dist/` |
| `npm run tauri:build` | 通过，产出 `target/release/lite-rtmp-streamer.exe` 与 NSIS 安装包 |
| 启动冒烟 | 进程存活、窗口标题 “Lite RTMP Streamer” 正常创建 |

## 6. 未验证项

- 真实平台**完整开播**（服务端接受并持续接收）：由于密钥侧条件不满足（见第 4 节），尚未取得“长时间不断流”的实录。
- WGC / GDI 后端的实机画面：代码路径已完成，未逐一在真机对每个后端做长时间稳定性压测。
- 预览与主播后台的**内嵌 canvas 渲染与系统浏览器打开**已在真机确认（`LITE_STREAM_TEST=1` 假地址启动引擎后勾选预览，主窗口 canvas 出实时桌面画面；窗口清单中始终只有主窗口一个可见窗口）。

## 7. 码率 / 音频 / 硬件编码实测（本轮）

| 项目 | 结论 |
| --- | --- |
| 设 8000 kbps 实际只有约 2400 | 根因：只设了 `CODECAPI_AVEncCommonRateControlMode`，**没设 `CODECAPI_AVEncCommonMeanBitRate`**，而 `MF_MT_AVG_BITRATE` 多数 MFT 不读。已按模式补设均值/峰值：CBR 均值=峰值=目标；ABR 均值=目标、峰值=2×；VBR 均值=0.8×、峰值=3×，并在日志打印“码率控制：X，目标 Y kbps” |
| ABR 静态画面码率明显低于目标 | 属正常行为：ABR 只保证**平均**码率，静止画面编码器自然降到 1 Mbps 级；动起来才会逼近目标 |
| 默认码率控制模式 | 已改为 **ABR**（`VideoConfig::default` 与 UI 下拉都默认 ABR） |
| 麦克风无声 | 早期 `mixer.rs` 把系统内录与麦克风的采样**首尾相接塞进同一条环**，时间轴错位。已重写为**双环混音**：系统轨 / 麦克风轨各一条队列，取 1024 采样 AAC 帧时两路相加再限幅；任一路有数据即出帧；`session.rs` 补静音只补系统轨 |
| 麦克风默认开关 | UI 的“麦克风”复选框已改为默认勾选 |
| 麦克风静音诊断 | 麦克风连续 2 秒只有静音数据时打印一次警告（提示 Windows 隐私设置、系统静音、选错输入设备） |
| 软件编码吞吐 | `gpu_encode_test`（1920×1080@30，ABR 8000）软件编码 265 fps（合成渐变帧，内容简单，仅证明不构成瓶颈） |
| NVENC（`NVIDIA H.264 Encoder MF`） | **激活阶段即失败**：`Scene`/ALL、`HARDWARE`、`HARDWARE｜SORT` 三种枚举方式都是 `0x8000FFFF 灾难性故障`，与输入类型、纹理无关 → 驱动层问题，代码无法绕过 |
| Intel QSV | 可 `ActivateObject`；但一旦 `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, token)` 就 **访问冲突崩溃**（已按厂商挑 Intel 适配器、设备加 `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`、开 `ID3D10Multithread` 多线程保护，仍崩）→ 本机 QSV 的 MF 封装不可用 |
| GPU 纹理直通代码 | 已实现（`encode/d3d.rs`：D3D11 设备 + `IMFDXGIDeviceManager` + NV12 纹理 + `MFCreateDXGISurfaceBuffer` + `UpdateSubresource`），但**必须 `LITE_GPU=1` 显式开启**，默认永不进入（避免崩溃） |


## 8. NVENC 直连（本轮结论，推翻第 7 节“硬件编码不可用”）

| 项目 | 结论 |
| --- | --- |
| 路线 | 放弃 Media Foundation 硬件 MFT（`SET_D3D_MANAGER` 必崩），改为**直连驱动自带 `nvEncodeAPI64.dll`**（`mini-nvcodec` 运行时 dlopen，不引入 CUDA / Video Codec SDK 构建依赖） |
| 会话打开 | `nvEncOpenEncodeSessionEx(DIRECTX)` 报 `NVENCSTATUS(15)=NV_ENC_ERR_INVALID_VERSION` 的唯一原因：`apiVersion` 必须等于加载器协商成功的 `api.created_with`，自选任何一档都失败 |
| 吞吐 | 1920×1080@60 合成帧 120/120 帧，250–270 fps；关键帧 1（GOP=2s），`extradata=true` |
| 码率控制 | 先清掉 P4 预设残留的 `targetQuality / targetQualityLSB`（非 0 时 NVENC 按固定质量出码，忽略 bitRate）；再按模式设 `averageBitRate / maxBitRate / vbvBufferSize`。按**媒体时间**折算后 8000kbps 目标实测约 9 Mbps，属噪声画面下正常偏差 |
| 端到端 | `e2e` 走真实链路（DXGI 采集 + 双轨音频 + 自研 RTMP，连本机接收器）：`EncoderInfo { name: "NVIDIA NVENC（硬件）", hardware: true }`，编码帧率稳定 30fps，服务端 `Publish.Start` 已接受 |
| 产品接入 | `VideoEncoder` 枚举（Nvenc / Mf）自动择优，失败回退；UI 复选框“NVIDIA 硬件编码（NVENC）”默认勾选；config `prefer_nvenc` 默认 true |
| 注意 | Optimus 笔记本上若进程落到集显，NVENC 初始化会失败；把 exe 路径写入 `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`，值 `GpuPreference=2;` 可强制走独显（需重启程序生效） |

## 9. GPU 色彩转换 / 缩放（本轮）

| 项目 | 结论 |
| --- | --- |
| 动机 | CPU 侧 `bgra_to_nv12` 实测 1920×1080 = 3.50 ms/帧；用户真实链路（2560×1440 采集 → 1920×1080 推流）还要叠加 CPU 缩放 3.28 ms，合计 7.05 ms/帧，60fps 下吃掉单核约 42% |
| 实现 | `encode/d3d.rs` 新增 `BgraConverter`：D3D11 Video Processor + BGRA 输入纹理 + NV12 输出视图，一步完成色彩空间转换与缩放；`NvencH264Encoder::encode_bgra` 直接吃采集原始帧，`submit_frame()` 复用原有的映射/编码/取位流逻辑 |
| 数值校验 | `gpu_convert_test`：纯红 Y=82 U=90 V=240、纯灰 Y=126 U=128 V=128、纯白 Y=235 U=128 V=128（BT.601 limited range，与 CPU 路径一致） |
| 耗时 | 640×360 0.30 ms、1920×1080 0.95 ms、2560×1440 1.43 ms（均含一次 CPU 行内存上传） |
| 端到端 | `e2e`（2560×1440 采集 → 1280×720 推流）：`EncoderInfo { NVIDIA NVENC（硬件） }`，编码 27.8fps/目标 30，`Publish.Start` 接受，服务端持续收流 |
| 回退 | 只有 NVENC 走 GPU 转换；回退到 Media Foundation（软件或硬件 MFT）时仍用 CPU 转换，行为与之前一致 |
| 仍未消除的开销 | 每帧仍有一次 CPU→显存的内存上传（1440p 约 14.7 MB/帧）。真正的零拷贝需要 DXGI 复制出的纹理直接交给编码器，属路线图下一项 |
