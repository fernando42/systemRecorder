//! 基础音频处理模块 (方案一)
//! 提供：高通滤波、噪声门、简单压缩器

use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DspSettings {
    pub enable_hpf: bool,
    pub hpf_cutoff: f32,      // Hz
    pub enable_gate: bool,
    pub gate_threshold: f32,  // 线性幅值 (0.0 ~ 1.0), e.g., 0.001 (-60dB)
    pub enable_compressor: bool,
    pub comp_threshold: f32,  // 线性幅值, e.g., 0.5
    pub comp_ratio: f32,      // 压缩比, e.g., 4.0 (4:1)
}

impl Default for DspSettings {
    fn default() -> Self {
        Self {
            enable_hpf: false,
            hpf_cutoff: 80.0,
            enable_gate: false,
            gate_threshold: 0.001,
            enable_compressor: false,
            comp_threshold: 0.5,
            comp_ratio: 4.0,
        }
    }
}

pub struct DspProcessor {
    settings: DspSettings,
    sample_rate: f32,
    // 每个通道的滤波器状态
    hpf_prev_x: Vec<f32>,
    hpf_prev_y: Vec<f32>,
}

impl DspProcessor {
    pub fn new(settings: DspSettings, sample_rate: u32, channels: u16) -> Self {
        Self {
            settings,
            sample_rate: sample_rate as f32,
            hpf_prev_x: vec![0.0; channels as usize],
            hpf_prev_y: vec![0.0; channels as usize],
        }
    }

    pub fn update_settings(&mut self, settings: DspSettings) {
        self.settings = settings;
    }

    /// 处理一帧音频数据 (交织采样 f32)
    pub fn process_frame(&mut self, data: &mut [f32]) {
        let channels = self.hpf_prev_x.len();
        if data.is_empty() { return; }

        // 1. 高通滤波 (High Pass Filter) - 一阶 IIR
        if self.settings.enable_hpf {
            let rc = 1.0 / (2.0 * std::f32::consts::PI * self.settings.hpf_cutoff);
            let dt = 1.0 / self.sample_rate;
            let alpha = rc / (rc + dt);

            for i in 0..data.len() {
                let ch = i % channels;
                let x = data[i];
                // y[n] = alpha * (y[n-1] + x[n] - x[n-1])
                let y = alpha * (self.hpf_prev_y[ch] + x - self.hpf_prev_x[ch]);
                
                self.hpf_prev_x[ch] = x;
                self.hpf_prev_y[ch] = y;
                data[i] = y;
            }
        }

        // 2. 噪声门 (Noise Gate)
        if self.settings.enable_gate {
            let threshold = self.settings.gate_threshold;
            for s in data.iter_mut() {
                if s.abs() < threshold {
                    *s = 0.0;
                }
            }
        }

        // 3. 简单压缩器 (Simple Compressor)
        if self.settings.enable_compressor {
            let thresh = self.settings.comp_threshold;
            let ratio = self.settings.comp_ratio;
            for s in data.iter_mut() {
                let abs_s = s.abs();
                if abs_s > thresh {
                    let sign = s.signum();
                    // 压缩部分: threshold + (original - threshold) / ratio
                    *s = sign * (thresh + (abs_s - thresh) / ratio);
                }
            }
        }
    }
}