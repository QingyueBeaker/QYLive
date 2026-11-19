# v0.1.0 — 首个公开版本

极简低耗的 Windows RTMP 推流工具。纯本地、无后台、无上报，MIT 协议开源。

## 下载

| 文件 | 尺寸 | SHA-256 |
| --- | --- | --- |
| `Lite RTMP Streamer_0.1.0_x64-setup.exe`（推荐，NSIS 安装包） | 1.83 MiB | `D17A83222D508CB2D71EC82E4FFA91E9250FE5FD577CE81A4127A99D5FBAAD09` |
| `lite-rtmp-streamer.exe`（免安装单文件，需系统已装 WebView2） | 6.8 MiB | `9917E6C6B29FBF6131AEBDB02B5C2F222416269689B0702A43579A41D9A60557` |

校验方式：

```powershell
Get-FileHash ".\Lite RTMP Streamer_0.1.0_x64-setup.exe" -Algorithm SHA256
```

## 本版特性

- **NVENC 直连硬件编码**：运行时加载驱动自带 `nvEncodeAPI64.dll`，不依赖 CUDA / Video Codec SDK；1080p60 合成帧实测 250+ fps；无 N 卡自动回退 Media Foundation，保证任何机器都能播。
- **GPU 色彩转换 + 缩放**：采集的 BGRA 帧交给 D3D11 Video Processor，在显存里完成 BGRA→NV12 与分辨率缩放，省掉每帧 3.5–7 ms 的 CPU 逐像素运算。
- **预览按需开关**：关闭时预览分支不执行，0 开销；预览分辨率与帧率可单独限流。
- **采集自动降级 + 热切换**：DXGI 桌面复制 → Windows Graphics Capture → GDI，切换过程中 RTMP 长连接不重连、音频不断。
- **单条 RTMP 长连接**：自研纯 Rust RTMP/FLV 客户端，只在网络真正断开时退避重连。
- **双轨音频混音**：WASAPI 系统内录 + 麦克风，独立增益，取 1024 采样合成 AAC 帧。
- **码率控制**：CBR / ABR（默认）/ VBR 三档，均值与峰值都下发到编码器。

## 使用（三步）

1. 在官方主播后台（或你用的平台后台）复制 RTMP 地址与串流密钥。
2. 粘进程序的两个输入框（整条 `地址?streamname=…&key=…` 也可以直接粘，程序会自动拆分）。
3. 点“开始推流”，状态栏会显示实际使用的编码器与硬件/软件。

> 粉丝数门槛、开播权限由平台决定，本工具不绕过任何平台限制。

## 已知限制

- 仅 Windows 10/11 x64；采集与编码深度绑定 DXGI / WGC / WASAPI / Media Foundation。
- 独占全屏个别游戏取不到画面，自动降 GDI；建议无边框窗口。
- HDR 桌面按 SDR 处理，颜色偏淡属预期。
- 每帧仍有一次 CPU→显存上传；真正的零拷贝在路线图中。

## 合规

本项目与哔哩哔哩无任何从属或合作关系，不含任何平台私有接口调用；不抓取密钥、不注入 JS、不自动操作网页。所有平台账号操作与密钥填写由使用者本人完成。
