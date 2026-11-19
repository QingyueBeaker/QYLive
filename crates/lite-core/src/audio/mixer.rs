//! 混音环：系统内录与麦克风各自一条环，编码线程按 AAC 帧长（1024 采样）取出时相加。
//!
//! 注意：两路必须**按时间轴相加**，不能像早期版本那样把两路采样首尾相接塞进同一条环——
//! 那样时间轴会被错位成“系统 100ms、麦克风 100ms”交替出现，表现为声音断续、
//! 甚至整段听不到麦克风。

use std::collections::VecDeque;

pub const TARGET_RATE: u32 = 48_000;
pub const TARGET_CHANNELS: usize = 2;
pub const AAC_FRAME_SAMPLES: usize = 1024;

/// 每条环最多缓存 1 秒（超出丢弃最旧数据，避免音画不同步无限累积）。
const MAX_FRAMES: usize = TARGET_RATE as usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Track {
    System,
    Mic,
}

pub struct Mixer {
    system: VecDeque<f32>,
    mic: VecDeque<f32>,
    /// 已产出的总采样帧数（用于时间戳）
    pub frames_out: u64,
    pub dropped_frames: u64,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            system: VecDeque::with_capacity(MAX_FRAMES * TARGET_CHANNELS),
            mic: VecDeque::with_capacity(MAX_FRAMES * TARGET_CHANNELS),
            frames_out: 0,
            dropped_frames: 0,
        }
    }

    fn ring_mut(&mut self, track: Track) -> &mut VecDeque<f32> {
        match track {
            Track::System => &mut self.system,
            Track::Mic => &mut self.mic,
        }
    }

    /// 写入交错双声道 f32（每条音轨独立排队，取帧时才相加）。
    pub fn push_stereo(&mut self, track: Track, samples: &[f32]) {
        let frames = samples.len() / TARGET_CHANNELS;
        let ring = self.ring_mut(track);
        for i in 0..frames {
            ring.push_back(samples[i * TARGET_CHANNELS]);
            ring.push_back(samples[i * TARGET_CHANNELS + 1]);
        }
        let mut dropped = 0u64;
        while ring.len() > MAX_FRAMES * TARGET_CHANNELS {
            ring.drain(..TARGET_CHANNELS);
            dropped += 1;
        }
        self.dropped_frames += dropped;
    }

    /// 取一个 AAC 帧（1024 采样双声道，i16）。两路相加并限幅。
    /// 只要**任一路**有数据就出帧（另一路按 0 处理），保证另一条音轨断流时不会卡住整条音频。
    pub fn take_aac_frame(&mut self, out: &mut Vec<i16>) -> Option<u64> {
        let need = AAC_FRAME_SAMPLES * TARGET_CHANNELS;
        let sys_len = self.system.len();
        let mic_len = self.mic.len();
        if sys_len < need && mic_len < need {
            return None;
        }
        out.clear();
        out.reserve(need);
        for _ in 0..need {
            let s = self.system.pop_front().unwrap_or(0.0);
            let m = self.mic.pop_front().unwrap_or(0.0);
            // 两路相加后软限幅，避免系统声音 + 麦克风同时大音量时削顶破音
            let v = (s + m).clamp(-1.0, 1.0);
            out.push((v * 32767.0) as i16);
        }
        let ts_samples = self.frames_out;
        self.frames_out += AAC_FRAME_SAMPLES as u64;
        Some(ts_samples)
    }

    /// 各音轨当前缓存帧数（诊断用）。
    pub fn buffered(&self) -> (usize, usize) {
        (
            self.system.len() / TARGET_CHANNELS,
            self.mic.len() / TARGET_CHANNELS,
        )
    }

    /// 立即峰值（诊断用）。
    pub fn peak(&self) -> f32 {
        self.system
            .iter()
            .chain(self.mic.iter())
            .fold(0.0f32, |a, b| a.max(b.abs()))
    }
}

/// 采样数 -> 毫秒
pub fn samples_to_ms(samples: u64, rate: u32) -> u32 {
    ((samples as f64 * 1000.0) / rate as f64) as u32
}
