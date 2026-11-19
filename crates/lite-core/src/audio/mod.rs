//! 音频采集与混音。
//!
//! 双轨：系统内录（WASAPI loopback）+ 麦克风（WASAPI capture），
//! 统一转成 48kHz / 双声道 / f32 后进入共享混音环，供 AAC 编码线程按 1024 采样块取用。
//! 视频采集切换、预览开关都不会触碰音频线程，因此音频永久连贯。

pub mod mixer;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::config::AudioConfig;
use crate::events::EngineEvent;
use crate::stats::Stats;
use mixer::{Mixer, TARGET_CHANNELS, TARGET_RATE};

/// 运行期可调的音量（千分比，1000 = 1.0），UI 拖动滑块实时生效。
#[derive(Clone)]
pub struct AudioVolumes {
    pub system: Arc<AtomicU32>,
    pub mic: Arc<AtomicU32>,
}

impl AudioVolumes {
    pub fn new(cfg: &AudioConfig) -> Self {
        Self {
            system: Arc::new(AtomicU32::new((cfg.system_volume * 1000.0) as u32)),
            mic: Arc::new(AtomicU32::new((cfg.mic_volume * 1000.0) as u32)),
        }
    }
    pub fn system_gain(&self) -> f32 {
        self.system.load(Ordering::Relaxed) as f32 / 1000.0
    }
    pub fn mic_gain(&self) -> f32 {
        self.mic.load(Ordering::Relaxed) as f32 / 1000.0
    }
}

pub struct AudioRuntime {
    pub stop: Arc<AtomicBool>,
    pub join: Option<JoinHandle<()>>,
    pub system_alive: Arc<AtomicBool>,
    pub mic_alive: Arc<AtomicBool>,
}

/// 启动音频采集（系统内录 + 可选麦克风）。返回的线程内部会自行做失败降级。
pub fn spawn_audio(
    cfg: AudioConfig,
    mixer: Arc<std::sync::Mutex<Mixer>>,
    volumes: AudioVolumes,
    stats: Stats,
    events: tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> AudioRuntime {
    let system_alive = Arc::new(AtomicBool::new(false));
    let mic_alive = Arc::new(AtomicBool::new(false));
    let sys_flag = system_alive.clone();
    let mic_flag = mic_alive.clone();
    let stop_for_thread = stop.clone();

    let join = std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let mut handles: Vec<JoinHandle<()>> = Vec::new();
            let mut note = String::new();

            if cfg.enabled && cfg.system_enabled {
                let m = mixer.clone();
                let v = volumes.clone();
                let st = stop_for_thread.clone();
                let flag = sys_flag.clone();
                let h = std::thread::Builder::new()
                    .name("audio-system".into())
                    .spawn(move || {
                        if let Err(e) = capture_endpoint(Endpoint::System, m, v, st, flag.clone()) {
                            log::warn!("系统内录失败：{e}");
                            flag.store(false, Ordering::Relaxed);
                        }
                    });
                match h {
                    Ok(h) => handles.push(h),
                    Err(e) => note = format!("系统内录线程创建失败: {e}"),
                }
            }

            if cfg.enabled && cfg.mic_enabled {
                let m = mixer.clone();
                let v = volumes.clone();
                let st = stop_for_thread.clone();
                let flag = mic_flag.clone();
                let h = std::thread::Builder::new()
                    .name("audio-mic".into())
                    .spawn(move || {
                        if let Err(e) = capture_endpoint(Endpoint::Microphone, m, v, st, flag.clone()) {
                            log::warn!("麦克风采集失败：{e}");
                            flag.store(false, Ordering::Relaxed);
                        }
                    });
                match h {
                    Ok(h) => handles.push(h),
                    Err(e) => note = format!("麦克风线程创建失败: {e}"),
                }
            }

            // 等第一份数据上来后再上报状态
            std::thread::sleep(Duration::from_millis(400));
            let system = sys_flag.load(Ordering::Relaxed);
            let mic = mic_flag.load(Ordering::Relaxed);
            let _ = events.send(EngineEvent::AudioStatus {
                system,
                mic,
                note: note.clone(),
            });
            let _ = stats;

            for h in handles {
                let _ = h.join();
            }
            unsafe { CoUninitialize() };
        })
        .expect("创建音频线程失败");

    AudioRuntime {
        stop,
        join: Some(join),
        system_alive,
        mic_alive,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    System,
    Microphone,
}

/// 采集一个 WASAPI 端点，转码成 48k 立体声 f32 后推入混音环。
fn capture_endpoint(
    endpoint: Endpoint,
    mixer: Arc<std::sync::Mutex<Mixer>>,
    volumes: AudioVolumes,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
) -> Result<(), String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("创建音频设备枚举器失败: {e}"))?;
        let (flow, loopback) = match endpoint {
            Endpoint::System => (eRender, true),
            Endpoint::Microphone => (eCapture, false),
        };
        let device = enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .map_err(|e| format!("获取默认音频端点失败（可能没有音频设备）: {e}"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("激活音频客户端失败: {e}"))?;
        let pwfx = client
            .GetMixFormat()
            .map_err(|e| format!("读取混音格式失败: {e}"))?;
        let wfx = *pwfx;

        let flags: u32 = if loopback {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_NOPERSIST
        };
        let buffer_100ns = 10_000_000i64 / 10; // 100ms
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                buffer_100ns,
                0,
                pwfx,
                None,
            )
            .map_err(|e| format!("初始化音频客户端失败: {e}"))?;

        let capture: IAudioCaptureClient = client
            .GetService::<IAudioCaptureClient>()
            .map_err(|e| format!("获取采集服务失败: {e}"))?;
        client.Start().map_err(|e| format!("启动音频采集失败: {e}"))?;

        let channels = wfx.nChannels as usize;
        let rate = wfx.nSamplesPerSec;
        let bits = wfx.wBitsPerSample as usize;
        let mut format = SampleFormat::detect(pwfx);
        if format == SampleFormat::Unknown {
            // 极少见格式：直接按 float32 处理，避免线程直接死掉
            let tag = wfx.wFormatTag;
            log::warn!("未知音频采样格式 tag={tag} bits={bits}，按 f32 处理");
            format = SampleFormat::F32;
        }

        alive.store(true, Ordering::Relaxed);
        let mut resampler = LinearResampler::new(rate as f64, TARGET_RATE as f64);
        let mut scratch: Vec<f32> = Vec::new();
        let mut out: Vec<f32> = Vec::new();
        // 麦克风静音诊断：连续 2 秒全是静音就提示一次
        let mut silent_ms: u32 = 0;
        let mut silent_logged = false;

        while !stop.load(Ordering::Relaxed) {
            let packet = match capture.GetNextPacketSize() {
                Ok(p) => p,
                Err(_) => break,
            };
            if packet == 0 {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }
            loop {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if frames > 0 {
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                        scratch.clear();
                        scratch.resize(frames as usize * TARGET_CHANNELS, 0.0);
                    } else {
                        format.decode(data, frames as usize, channels, &mut scratch);
                    }
                    let gain = match endpoint {
                        Endpoint::System => volumes.system_gain(),
                        Endpoint::Microphone => volumes.mic_gain(),
                    };
                    resampler.process(&scratch, channels, gain, &mut out);
                    if !out.is_empty() {
                        if endpoint == Endpoint::Microphone {
                            if out.iter().all(|v| v.abs() < 1e-6) {
                                silent_ms += 20;
                                if silent_ms >= 2000 && !silent_logged {
                                    silent_logged = true;
                                    log::warn!(
                                        "麦克风持续 2 秒只有静音数据：请检查 Windows 隐私设置（设置→隐私和安全性→麦克风，允许桌面应用访问）、麦克风是否被系统静音、以及是否选错默认输入设备"
                                    );
                                }
                            } else {
                                silent_ms = 0;
                            }
                        }
                        if let Ok(mut m) = mixer.lock() {
                            m.push_stereo(
                                match endpoint {
                                    Endpoint::System => mixer::Track::System,
                                    Endpoint::Microphone => mixer::Track::Mic,
                                },
                                &out,
                            );
                        }
                        out.clear();
                    }
                }
                let _ = capture.ReleaseBuffer(frames);
                match capture.GetNextPacketSize() {
                    Ok(next) if next > 0 => continue,
                    _ => break,
                }
            }
        }

        let _ = client.Stop();
        alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SampleFormat {
    F32,
    I16,
    I32,
    Unknown,
}

impl SampleFormat {
    fn detect(wfx: *const windows::Win32::Media::Audio::WAVEFORMATEX) -> Self {
        unsafe {
            let w = *wfx;
            // WAVE_FORMAT_IEEE_FLOAT = 3, WAVE_FORMAT_PCM = 1, WAVE_FORMAT_EXTENSIBLE = 0xFFFE
            match w.wFormatTag {
                3 => SampleFormat::F32,
                1 => match w.wBitsPerSample {
                    16 => SampleFormat::I16,
                    32 => SampleFormat::I32,
                    _ => SampleFormat::Unknown,
                },
                0xFFFE => {
                    let ext_ptr = wfx as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE;
                    // WAVEFORMATEXTENSIBLE 是 packed 结构，必须用 read_unaligned 读取 SubFormat
                    let sub: windows::core::GUID =
                        std::ptr::read_unaligned(std::ptr::addr_of!((*ext_ptr).SubFormat));
                    if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                        SampleFormat::F32
                    } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                        match w.wBitsPerSample {
                            16 => SampleFormat::I16,
                            32 => SampleFormat::I32,
                            _ => SampleFormat::Unknown,
                        }
                    } else {
                        SampleFormat::Unknown
                    }
                }
                _ => SampleFormat::Unknown,
            }
        }
    }

    /// 解码为交错 f32（保持原始声道数）。
    fn decode(&self, data: *mut u8, frames: usize, channels: usize, out: &mut Vec<f32>) {
        out.clear();
        let count = frames * channels;
        unsafe {
            match self {
                SampleFormat::F32 => {
                    let src = std::slice::from_raw_parts(data as *const f32, count);
                    out.extend_from_slice(src);
                }
                SampleFormat::I16 => {
                    let src = std::slice::from_raw_parts(data as *const i16, count);
                    out.extend(src.iter().map(|v| *v as f32 / 32768.0));
                }
                SampleFormat::I32 => {
                    let src = std::slice::from_raw_parts(data as *const i32, count);
                    out.extend(src.iter().map(|v| *v as f32 / 2147483648.0));
                }
                SampleFormat::Unknown => out.resize(count, 0.0),
            }
        }
    }
}

/// 线性插值重采样 + 声道折叠到双声道。
pub struct LinearResampler {
    ratio: f64,
    pos: f64,
    tail: Vec<f32>,
}

impl LinearResampler {
    pub fn new(src: f64, dst: f64) -> Self {
        Self {
            ratio: src / dst,
            pos: 0.0,
            tail: Vec::new(),
        }
    }

    /// input：交错原始声道；输出：交错双声道 48k
    pub fn process(&mut self, input: &[f32], src_channels: usize, gain: f32, out: &mut Vec<f32>) {
        if src_channels == 0 {
            return;
        }
        let src_frames = input.len() / src_channels;
        if src_frames == 0 {
            return;
        }

        // 拼上上一批遗留的最后一个采样点，保证跨块连续
        let mut work: Vec<f32> = Vec::with_capacity(self.tail.len() + input.len());
        work.extend_from_slice(&self.tail);
        work.extend_from_slice(input);
        let ch = src_channels;

        let mut i = self.pos;
        while (i as usize) + 1 < src_frames + self.tail.len() / ch {
            let idx = i as usize;
            let frac = (i - idx as f64) as f32;
            let f0 = idx * ch;
            let f1 = (idx + 1) * ch;
            if f1 + ch > work.len() {
                break;
            }
            let (l0, r0) = fold(&work[f0..f0 + ch]);
            let (l1, r1) = fold(&work[f1..f1 + ch]);
            out.push((l0 + (l1 - l0) * frac) * gain);
            out.push((r0 + (r1 - r0) * frac) * gain);
            i += self.ratio;
        }
        // 保留最后一个采样点作为下一批的插值起点
        let consumed_frames = (i as usize).min(src_frames + self.tail.len() / ch - 1);
        let consumed = consumed_frames * ch;
        self.tail = work[consumed..].to_vec();
        self.pos = i - consumed_frames as f64;
    }
}

/// 任意声道折叠成 (L, R)。
fn fold(frame: &[f32]) -> (f32, f32) {
    match frame.len() {
        0 => (0.0, 0.0),
        1 => (frame[0], frame[0]),
        2 => (frame[0], frame[1]),
        n => {
            // 5.1 等：取前两声道（L/R），其余忽略
            let _ = n;
            (frame[0], frame[1])
        }
    }
}

pub const _AUDIO_CHANNELS: usize = TARGET_CHANNELS;
