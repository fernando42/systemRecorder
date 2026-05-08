//! 基础音频处理模块
//! 提供：高通滤波、RNNoise实时降噪、噪声门、简单压缩器

use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DspSettings {
    pub enable_hpf: bool,
    pub hpf_cutoff: f32,      // Hz
    pub enable_denoise: bool, // RNNoise 实时降噪
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
            enable_denoise: false,
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

    // RNNoise 状态
    denoisers: Vec<Box<nnnoiseless::DenoiseState<'static>>>,
    input_buffers: Vec<Vec<f32>>,  // 待处理缓冲 (每个通道)
    output_buffers: Vec<Vec<f32>>, // 处理后缓冲 (每个通道)
}

impl DspProcessor {
    pub fn new(settings: DspSettings, sample_rate: u32, channels: u16) -> Self {
        let mut denoisers = Vec::with_capacity(channels as usize);
        let mut input_buffers = Vec::with_capacity(channels as usize);
        let mut output_buffers = Vec::with_capacity(channels as usize);

        for _ in 0..channels {
            denoisers.push(nnnoiseless::DenoiseState::new());
            input_buffers.push(Vec::with_capacity(960)); // 足够容纳两个 frame
            output_buffers.push(Vec::with_capacity(960));
        }

        Self {
            settings,
            sample_rate: sample_rate as f32,
            hpf_prev_x: vec![0.0; channels as usize],
            hpf_prev_y: vec![0.0; channels as usize],
            denoisers,
            input_buffers,
            output_buffers,
        }
    }

    pub fn update_settings(&mut self, settings: DspSettings) {
        self.settings = settings;
    }

    /// 处理一帧音频数据 (交织采样 f32)
    pub fn process_frame(&mut self, data: &mut [f32]) {
        let channels = self.hpf_prev_x.len();
        if data.is_empty() { return; }

        // --- 阶段 1: 高通滤波 (HPF) ---
        if self.settings.enable_hpf {
            let rc = 1.0 / (2.0 * std::f32::consts::PI * self.settings.hpf_cutoff);
            let dt = 1.0 / self.sample_rate;
            let alpha = rc / (rc + dt);

            for i in 0..data.len() {
                let ch = i % channels;
                let x = data[i];
                let y = alpha * (self.hpf_prev_y[ch] + x - self.hpf_prev_x[ch]);
                self.hpf_prev_x[ch] = x;
                self.hpf_prev_y[ch] = y;
                data[i] = y;
            }
        }

        // --- 阶段 2: RNNoise 实时降噪 (块处理) ---
        if self.settings.enable_denoise {
            // 1. 将输入数据分发到各通道缓冲
            for i in 0..data.len() {
                let ch = i % channels;
                self.input_buffers[ch].push(data[i]);
            }

            // 2. 处理所有满 480 采样的块
            for ch in 0..channels {
                while self.input_buffers[ch].len() >= 480 {
                    let mut frame = [0.0f32; 480];
                    // 取出前 480 个采样
                    for j in 0..480 {
                        frame[j] = self.input_buffers[ch][j];
                    }
                    
                    let mut out_frame = [0.0f32; 480];
                    self.denoisers[ch].process_frame(&mut out_frame, &frame);
                    
                    // 将处理结果存入输出缓冲
                    self.output_buffers[ch].extend_from_slice(&out_frame);
                    
                    // 移除已处理的采样
                    self.input_buffers[ch].drain(0..480);
                }
            }

            // 3. 用输出缓冲尝试覆盖当前 data (延迟生效)
            // 为了保持实时流长度不变，我们从 output_buffers 中提取相同数量的采样。
            for i in 0..data.len() {
                let ch = i % channels;
                if !self.output_buffers[ch].is_empty() {
                    data[i] = self.output_buffers[ch].remove(0);
                }
                // 如果输出缓冲不足，则保留原值（或静音)，但由于是录制且延迟固定，通常能填满。
            }
            
            // 4. 清理缓冲区：防止无限增长，限制最大缓冲区大小
            // RNNoise 延迟约为 480 采样 (10ms @ 48kHz)，保留 2 倍延迟缓冲即可
            const MAX_BUFFER_SIZE: usize = 960;
            for ch in 0..channels {
                if self.input_buffers[ch].len() > MAX_BUFFER_SIZE {
                    self.input_buffers[ch].truncate(MAX_BUFFER_SIZE);
                }
                if self.output_buffers[ch].len() > MAX_BUFFER_SIZE {
                    self.output_buffers[ch].truncate(MAX_BUFFER_SIZE);
                }
            }
        }

        // --- 阶段 3: 噪声门 (Noise Gate) ---
        if self.settings.enable_gate {
            let threshold = self.settings.gate_threshold;
            for s in data.iter_mut() {
                if s.abs() < threshold {
                    *s = 0.0;
                }
            }
        }

        // --- 阶段 4: 简单压缩器 (Simple Compressor) ---
        if self.settings.enable_compressor {
            let thresh = self.settings.comp_threshold;
            let ratio = self.settings.comp_ratio;
            for s in data.iter_mut() {
                let abs_s = s.abs();
                if abs_s > thresh {
                    let sign = s.signum();
                    *s = sign * (thresh + (abs_s - thresh) / ratio);
                }
            }
        }
    }
}