import "./app.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Backend = "dxgi" | "wgc" | "gdi";

interface MonitorInfo {
  index: number;
  name: string;
  width: number;
  height: number;
  primary: boolean;
}

interface WindowInfo {
  hwnd: number;
  title: string;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const rtmpUrl = $<HTMLInputElement>("rtmpUrl");
const streamKey = $<HTMLInputElement>("streamKey");
const autoDegrade = $<HTMLInputElement>("autoDegrade");
const monitorSel = $<HTMLSelectElement>("monitorSel");
const backendSel = $<HTMLSelectElement>("backendSel");
const windowSel = $<HTMLSelectElement>("windowSel");
const resolution = $<HTMLSelectElement>("resolution");
const fpsSel = $<HTMLSelectElement>("fps");
const bitrate = $<HTMLInputElement>("bitrate");
const gop = $<HTMLInputElement>("gop");
const rateControl = $<HTMLSelectElement>("rateControl");
const nvenc = $<HTMLInputElement>("nvenc");
const sysAudio = $<HTMLInputElement>("sysAudio");
const sysGain = $<HTMLInputElement>("sysGain");
const micAudio = $<HTMLInputElement>("micAudio");
const micGain = $<HTMLInputElement>("micGain");
const audioBitrate = $<HTMLSelectElement>("audioBitrate");
const previewToggle = $<HTMLInputElement>("previewToggle");
const previewWidth = $<HTMLSelectElement>("previewWidth");
const previewFps = $<HTMLSelectElement>("previewFps");
const previewBox = $<HTMLDivElement>("previewBox");
const previewCanvas = $<HTMLCanvasElement>("previewCanvas");
const previewCtx = previewCanvas.getContext("2d", { alpha: false });

const btnStart = $<HTMLButtonElement>("btnStart");
const btnStop = $<HTMLButtonElement>("btnStop");
const btnPanel = $<HTMLButtonElement>("btnPanel");
const btnRefresh = $<HTMLButtonElement>("btnRefresh");

const statusDot = $<HTMLSpanElement>("statusDot");
const statusText = $<HTMLSpanElement>("statusText");
const logBox = $<HTMLDivElement>("log");

const statSource = $<HTMLSpanElement>("statSource");
const statEncoder = $<HTMLSpanElement>("statEncoder");
const statFps = $<HTMLSpanElement>("statFps");
const statKbps = $<HTMLSpanElement>("statKbps");
const statFrames = $<HTMLSpanElement>("statFrames");
const statSwitch = $<HTMLSpanElement>("statSwitch");
const statReconnect = $<HTMLSpanElement>("statReconnect");
const statAudio = $<HTMLSpanElement>("statAudio");
const statRtmp = $<HTMLSpanElement>("statRtmp");
const statHint = $<HTMLSpanElement>("statHint");

/** 顶部状态栏旁的显眼提示：校验失败、启动失败都会写这里，避免“点了没反应” */
function setHint(text: string) {
  statHint.textContent = text;
  statHint.title = text;
}

function statusLabel(s: string) {
  switch (s) {
    case "connecting":
      return "连接中";
    case "publishing":
      return "已推流";
    case "retrying":
      return "重连中";
    case "failed":
      return "失败";
    case "stopped":
      return "已停止";
    default:
      return "空闲";
  }
}

/** 引擎已启动（采集/编码在跑），不代表服务端已接受推流 */
let engineOn = false;
/** 推流连接真实状态 */
let rtmpState: "idle" | "connecting" | "publishing" | "retrying" | "failed" | "stopped" = "idle";

function log(text: string, level: "info" | "warn" | "error" = "info") {
  const ts = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  const prefix = level === "error" ? "✕" : level === "warn" ? "!" : "·";
  logBox.textContent += `[${ts}] ${prefix} ${text}\n`;
  const lines = logBox.textContent.split("\n");
  if (lines.length > 300) logBox.textContent = lines.slice(-300).join("\n");
  logBox.scrollTop = logBox.scrollHeight;
}

function setRunning(on: boolean) {
  engineOn = on;
  refreshStatus();
}

/** 一次启动请求正在飞行中（防止连点），与“引擎是否在跑”无关 */
let starting = false;

/** 状态灯只反映“服务端是否真的在收流”，而不是“引擎有没有起来”。 */
function refreshStatus() {
  let dot = "bg-slate-600";
  let text = "未开播";
  let color = "text-slate-400";
  const published = engineOn && rtmpState === "publishing";
  if (!engineOn) {
    rtmpState = "idle";
  } else if (rtmpState === "publishing") {
    dot = "bg-emerald-500";
    text = "推流中";
    color = "text-emerald-400";
  } else if (rtmpState === "connecting") {
    dot = "bg-amber-400";
    text = "连接中";
    color = "text-amber-400";
  } else if (rtmpState === "retrying") {
    dot = "bg-amber-500";
    text = "重连中";
    color = "text-amber-400";
  } else if (rtmpState === "failed") {
    dot = "bg-rose-500";
    text = "推流失败";
    color = "text-rose-400";
  } else {
    dot = "bg-slate-500";
    text = "引擎运行中";
    color = "text-slate-300";
  }
  // 按钮永远可点（除了正在启动 / 已经真正推上去），避免“点了没反应”的死按钮
  btnStart.disabled = starting || published;
  btnStart.textContent = starting
    ? "启动中…"
    : published
      ? "推流中"
      : engineOn
        ? "重试连接"
        : "开始推流";
  btnStop.disabled = !engineOn;
  statusDot.className = `h-2 w-2 rounded-full ${dot}`;
  statusText.textContent = text;
  statusText.className = `text-[12px] ${color}`;
}

function readConfig() {
  const [w, h] = resolution.value.split("x").map((v) => Number(v));
  // 容错：用户把整条 «地址?streamname=...&key=...» 一起粘进地址框时，
  // 自动把 "?" 之后的部分当串流密钥，地址只保留协议+host+app。
  let url = rtmpUrl.value.trim();
  let key = streamKey.value.trim();
  const q = url.indexOf("?");
  if (q > 0) {
    if (!key) key = url.slice(q);
    url = url.slice(0, q);
  }
  // 关键：只有地址非空时才补末尾斜杠，否则空串会变成 "/"，绕过校验直达后端
  if (url && !url.endsWith("/")) url += "/";
  return {
    rtmp_url: url,
    stream_key: key,
    video: {
      width: w,
      height: h,
      fps: Number(fpsSel.value),
      bitrate_kbps: Number(bitrate.value) || 6000,
      gop_seconds: Number(gop.value) || 2,
      rate_control: rateControl.value,
      prefer_nvenc: nvenc.checked,
    },
    audio: {
      enabled: true,
      system_enabled: sysAudio.checked,
      system_volume: Number(sysGain.value) / 100,
      mic_enabled: micAudio.checked,
      mic_volume: Number(micGain.value) / 100,
      bitrate_kbps: Number(audioBitrate.value),
    },
    preview: {
      enabled: previewToggle.checked,
      max_width: Number(previewWidth.value),
      fps: Number(previewFps.value),
    },
    source: {
      auto: autoDegrade.checked,
      locked: backendSel.value as Backend,
      monitor: Number(monitorSel.value) || 0,
      hwnd: Number(windowSel.value) || 0,
    },
  };
}

async function refreshMonitors() {
  try {
    const list = await invoke<MonitorInfo[]>("list_monitors");
    monitorSel.innerHTML = "";
    if (!list.length) {
      const opt = document.createElement("option");
      opt.value = "0";
      opt.textContent = "未检测到显示器";
      monitorSel.appendChild(opt);
      return;
    }
    for (const m of list) {
      const opt = document.createElement("option");
      opt.value = String(m.index);
      opt.textContent = `#${m.index} ${m.name} ${m.width}×${m.height}${m.primary ? " (主)" : ""}`;
      monitorSel.appendChild(opt);
    }
  } catch (e) {
    log(`枚举显示器失败：${e}`, "warn");
  }
}

async function refreshWindows() {
  try {
    const list = await invoke<WindowInfo[]>("list_windows");
    const keep = windowSel.value;
    windowSel.innerHTML = "";
    const opt0 = document.createElement("option");
    opt0.value = "0";
    opt0.textContent = "整屏（不使用窗口捕获）";
    windowSel.appendChild(opt0);
    for (const w of list) {
      const opt = document.createElement("option");
      opt.value = String(w.hwnd);
      opt.textContent = w.title.length > 60 ? `${w.title.slice(0, 60)}…` : w.title;
      windowSel.appendChild(opt);
    }
    windowSel.value = keep;
  } catch (e) {
    log(`枚举窗口失败：${e}`, "warn");
  }
}

btnStart.addEventListener("click", async () => {
  if (starting) return;
  const cfg = readConfig();
  if (!cfg.rtmp_url) {
    setHint("请先粘贴 RTMP 推流地址（灰色文字只是示例）");
    log("请先粘贴 RTMP 推流地址", "error");
    rtmpUrl.focus();
    return;
  }
  if (!/^rtmps?:\/\//i.test(cfg.rtmp_url)) {
    setHint("推流地址必须以 rtmp:// 或 rtmps:// 开头（别贴直播页网址）");
    log("推流地址格式不对：需要 rtmp:// 或 rtmps:// 开头的推流地址", "error");
    rtmpUrl.focus();
    return;
  }
  if (!cfg.stream_key) {
    setHint("请先粘贴串流密钥（含 ?streamname=… 整段）");
    log("请先粘贴串流密钥", "error");
    streamKey.focus();
    return;
  }
  starting = true;
  setHint("");
  refreshStatus();
  log("正在启动引擎…");
  try {
    // 已在跑但没推上去（重连中/失败）时，允许“重试连接”：先干净停掉再按当前参数重开
    if (engineOn) {
      await invoke("stop_stream");
      setRunning(false);
      await new Promise((r) => setTimeout(r, 400));
    }
    await invoke("start_stream", { cfg });
    setRunning(true);
    log("引擎已启动，正在连接推流服务器…（状态栏变绿才代表服务端已收流）");
  } catch (e) {
    setHint(`开播失败：${e}`);
    log(`开播失败：${e}`, "error");
    setRunning(false);
  } finally {
    starting = false;
    refreshStatus();
  }
});

btnStop.addEventListener("click", async () => {
  try {
    await invoke("stop_stream");
    log("已停止推流");
  } catch (e) {
    log(`停播失败：${e}`, "error");
  }
  setRunning(false);
});

btnPanel.addEventListener("click", async () => {
  try {
    await invoke("open_live_panel");
    log("已打开直播中心：请登录 → 设置分区/标题 → 点“开始直播” → 再回来复制密钥");
  } catch (e) {
    log(`打开后台失败：${e}`, "error");
  }
});

const btnOpenBrowser = $<HTMLButtonElement>("btnOpenBrowser");
btnOpenBrowser.addEventListener("click", async () => {
  try {
    await invoke("open_in_browser", { url: "https://live.bilibili.com/" });
    log("已用系统浏览器打开主播后台");
  } catch (e) {
    log(`打开浏览器失败：${e}`, "error");
  }
});

// 粉丝不足 5000 时没有第三方开播权限：改用官方直播姬的“第三方推流模式”，
// 由直播姬完成对平台的推流，本程序只推它的本地入口。
const btnHime = $<HTMLButtonElement>("btnHime");
btnHime.addEventListener("click", () => {
  rtmpUrl.value = "rtmp://127.0.0.1:1935/";
  streamKey.value = "livehime";
  setHint("");
  log("已填入直播姬本地入口（rtmp://127.0.0.1:1935/ + livehime）：请先在 PC 直播姬左上角切到“第三方推流模式”并点开始直播，再回到这里开始推流；过程中别关直播姬");
});

btnRefresh.addEventListener("click", () => {
  void refreshMonitors();
  void refreshWindows();
});

autoDegrade.addEventListener("change", () => {
  backendSel.disabled = autoDegrade.checked;
});

previewToggle.addEventListener("change", async () => {
  try {
    await invoke("set_preview", {
      enabled: previewToggle.checked,
      maxWidth: Number(previewWidth.value),
      fps: Number(previewFps.value),
    });
    if (previewToggle.checked) startPreviewLoop();
    else stopPreviewLoop();
    log(previewToggle.checked ? "预览已开启" : "预览已关闭（零开销）");
  } catch (e) {
    log(`切换预览失败：${e}`, "error");
  }
});

/* ---------- 主窗口内嵌预览（不再开额外窗口） ---------- */
let previewTimer: number | null = null;
let previewSeq = -1;

function stopPreviewLoop() {
  if (previewTimer !== null) {
    window.clearTimeout(previewTimer);
    previewTimer = null;
  }
  previewBox.classList.add("hidden");
  previewCtx?.clearRect(0, 0, previewCanvas.width, previewCanvas.height);
}

function startPreviewLoop() {
  if (!previewCtx) {
    log("当前环境不支持 Canvas 预览", "warn");
    return;
  }
  previewBox.classList.remove("hidden");
  if (previewTimer !== null) return;
  const interval = Math.max(16, Math.round(1000 / Math.max(1, Number(previewFps.value))));
  const tick = async () => {
    // 窗口不可见时不拉帧：预览关闭时永远进不到这里
    if (!document.hidden && previewToggle.checked) {
      try {
        const raw = (await invoke("preview_frame")) as unknown as ArrayBuffer;
        if (raw && raw.byteLength > 12) {
          const head = new DataView(raw);
          const seq = Number(head.getBigUint64(0, true));
          const w = head.getUint16(8, true);
          const h = head.getUint16(10, true);
          if (seq !== previewSeq && w > 1 && h > 1) {
            previewSeq = seq;
            if (previewCanvas.width !== w || previewCanvas.height !== h) {
              previewCanvas.width = w;
              previewCanvas.height = h;
            }
            const pixels = new Uint8ClampedArray(raw, 12);
            previewCtx!.putImageData(new ImageData(pixels, w, h), 0, 0);
          }
        }
      } catch {
        /* 未开播时静默等待 */
      }
    }
    if (previewToggle.checked) previewTimer = window.setTimeout(tick, interval);
    else previewTimer = null;
  };
  void tick();
}

previewWidth.addEventListener("change", () => {
  if (previewToggle.checked)
    void invoke("set_preview", {
      enabled: true,
      maxWidth: Number(previewWidth.value),
      fps: Number(previewFps.value),
    });
});

previewFps.addEventListener("change", () => {
  if (previewToggle.checked)
    void invoke("set_preview", {
      enabled: true,
      maxWidth: Number(previewWidth.value),
      fps: Number(previewFps.value),
    });
});

sysGain.addEventListener("input", () => {
  void invoke("set_audio_volume", { which: "system", gain: Number(sysGain.value) / 100 });
});
micGain.addEventListener("input", () => {
  void invoke("set_audio_volume", { which: "mic", gain: Number(micGain.value) / 100 });
});

interface StatsPayload {
  video_frames: number;
  video_dropped: number;
  audio_frames: number;
  bytes_sent: number;
  capture_failures: number;
  switch_count: number;
  reconnect_count: number;
  encoder_fps: number;
  capture_fps: number;
  kbps: number;
  running: boolean;
}

type EngineEvent =
  | { type: "started"; width: number; height: number; fps: number }
  | { type: "stopped"; reason: string }
  | { type: "sourceChanged"; kind: string; desc: string; reason: string }
  | { type: "encoderInfo"; name: string; hardware: boolean; codec: string }
  | { type: "audioStatus"; system: boolean; mic: boolean; note: string }
  | { type: "stats"; stats: StatsPayload }
  | { type: "rtmpStatus"; state: string; detail: string }
  | { type: "log"; level: string; msg: string }
  | { type: "error"; msg: string };

async function bindEvents() {
  await listen<EngineEvent>("engine-event", (event) => {
    const ev = event.payload;
    switch (ev.type) {
      case "started":
        log(`推流参数 ${ev.width}×${ev.height}@${ev.fps}`);
        setRunning(true);
        break;
      case "stopped":
        log(`推流结束：${ev.reason}`);
        setRunning(false);
        break;
      case "sourceChanged":
        statSource.textContent = ev.desc;
        if (ev.reason.includes("热切换") || ev.reason.includes("降级")) log(`采集源变更：${ev.desc}（${ev.reason}）`, "warn");
        break;
      case "encoderInfo":
        statEncoder.textContent = `${ev.codec} · ${ev.name}${ev.hardware ? " (硬件)" : " (软件)"}`;
        break;
      case "audioStatus":
        statAudio.textContent = `${ev.system ? "系统内录 ✓" : "系统内录 ✕"} / ${
          ev.mic ? "麦克风 ✓" : "麦克风 ✕"
        }${ev.note ? " · " + ev.note : ""}`;
        if (!ev.system && !ev.mic) log("音频设备不可用，本次将只推视频", "warn");
        break;
      case "stats": {
        const s = ev.stats;
        statFps.textContent = `${s.capture_fps.toFixed(1)} / ${s.encoder_fps.toFixed(1)}`;
        statKbps.textContent = `${s.kbps.toFixed(0)} kbps`;
        statFrames.textContent = `${s.video_frames} / ${s.video_dropped}`;
        statSwitch.textContent = String(s.switch_count);
        statReconnect.textContent = String(s.reconnect_count);
        break;
      }
      case "rtmpStatus": {
        rtmpState = (ev.state as typeof rtmpState) ?? "idle";
        statRtmp.textContent = `${statusLabel(rtmpState)}${ev.detail ? " · " + ev.detail : ""}`;
        refreshStatus();
        if (rtmpState === "publishing") {
          setHint("");
          log("服务端已接受推流，直播间应已开播");
        } else if (rtmpState === "failed") {
          setHint(`推流连接失败：${ev.detail}`);
          log(`推流失败：${ev.detail}`, "error");
        } else if (rtmpState === "retrying") {
          setHint("连接被服务端断开，正在重连（多为未点“开始直播”或密钥过期）");
          log(`连接断开：${ev.detail}`, "warn");
        }
        break;
      }
      case "log":
        log(ev.msg, ev.level === "warn" ? "warn" : "info");
        break;
      case "error":
        log(ev.msg, "error");
        break;
    }
  });
}

async function init() {
  void bindEvents();
  await refreshMonitors();
  await refreshWindows();
  backendSel.disabled = autoDegrade.checked;
  try {
    const streaming = await invoke<boolean>("is_streaming");
    setRunning(streaming);
  } catch {
    setRunning(false);
  }
  const info = await invoke<{ name: string; version: string; platform: string }>("app_info");
  log(`${info.name} v${info.version} · ${info.platform}`);
  void info;
}

void init();
