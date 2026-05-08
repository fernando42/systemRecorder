//! 基础音频处理模块
//! 提供：高通滤波、RNNoise实时降噪、噪声门、简单压缩器、AGC、均衡器、限幅器

use serde::{Deserialize, Serialize};

/// 均衡器预设
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum EqPreset {
    VocalEnhance, // 人声增强
    Broadcast,    // 广播
    Phone,        // 电话
    Custom,       // 自定义
}

impl Default for EqPreset {
    fn default() -> Self {
        EqPreset::VocalEnhance
    }
}

/// 均衡器频段参数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBandSettings {
    pub center_freq: f32, // 中心频率 (Hz)
    pub q_factor: f32,    // Q值
    pub gain_db: f32,     // 增益 (dB)
}

/// 人声增强设置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VocalEnhancementSettings {
    // AGC 设置
    pub enable_agc: bool,
    pub agc_target_rms: f32,  // 目标 RMS 电平 (0.0-1.0)
    pub agc_attack_ms: f32,   // 攻击时间 (ms)
    pub agc_release_ms: f32,  // 释放时间 (ms)
    pub agc_max_gain_db: f32, // 最大增益 (dB)

    // 均衡器设置
    pub enable_eq: bool,
    pub eq_preset: EqPreset,
    pub eq_bands: Vec<EqBandSettings>,

    // 限幅器设置
    pub enable_limiter: bool,
    pub limiter_threshold_db: f32, // 阈值 (dBFS)
    pub limiter_attack_ms: f32,    // 攻击时间 (ms)
    pub limiter_release_ms: f32,   // 释放时间 (ms)
}

impl Default for VocalEnhancementSettings {
    fn default() -> Self {
        Self {
            enable_agc: true,
            agc_target_rms: 0.3,
            agc_attack_ms: 10.0,
            agc_release_ms: 100.0,
            agc_max_gain_db: 20.0,

            enable_eq: true,
            eq_preset: EqPreset::VocalEnhance,
            eq_bands: default_vocal_eq_bands(),

            enable_limiter: true,
            limiter_threshold_db: -1.0,
            limiter_attack_ms: 1.0,
            limiter_release_ms: 100.0,
        }
    }
}

/// 默认人声均衡器频段
fn default_vocal_eq_bands() -> Vec<EqBandSettings> {
    vec![
        // 低频切除 - 去除嗡嗡声
        EqBandSettings {
            center_freq: 80.0,
            q_factor: 0.7,
            gain_db: -6.0,
        },
        // 低频增强 - 增加温暖感
        EqBandSettings {
            center_freq: 200.0,
            q_factor: 1.0,
            gain_db: 2.0,
        },
        // 中频增强 - 人声核心频段
        EqBandSettings {
            center_freq: 1000.0,
            q_factor: 1.4,
            gain_db: 4.0,
        },
        // 高频增强 - 增加清晰度
        EqBandSettings {
            center_freq: 3000.0,
            q_factor: 1.0,
            gain_db: 3.0,
        },
        // 空气感 - 增加通透感
        EqBandSettings {
            center_freq: 8000.0,
            q_factor: 0.7,
            gain_db: 2.0,
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DspSettings {
    pub enable_hpf: bool,
    pub hpf_cutoff: f32,      // Hz
    pub enable_denoise: bool, // RNNoise 实时降噪
    pub enable_gate: bool,
    pub gate_threshold: f32, // 线性幅值 (0.0 ~ 1.0), e.g., 0.001 (-60dB)
    pub enable_compressor: bool,
    pub comp_threshold: f32, // 线性幅值, e.g., 0.5
    pub comp_ratio: f32,     // 压缩比, e.g., 4.0 (4:1)

    // 人声增强设置
    pub vocal_enhancement: VocalEnhancementSettings,
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
            vocal_enhancement: VocalEnhancementSettings::default(),
        }
    }
}

/// AGC 处理器状态
struct AgcState {
    current_gain: f32,
    current_rms: f32,
}

/// 二阶滤波器状态 (用于均衡器)
#[derive(Clone)]
struct BiquadFilter {
    prev_x1: f32,
    prev_x2: f32,
    prev_y1: f32,
    prev_y2: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    // a0 已归一化到 1.0
    a1: f32,
    a2: f32,
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

    // AGC 状态 (每个通道)
    agc_states: Vec<AgcState>,

    // 均衡器状态 (每个通道)
    eq_filters: Vec<Vec<BiquadFilter>>,
}

impl DspProcessor {
    pub fn new(settings: DspSettings, sample_rate: u32, channels: u16) -> Self {
        let mut denoisers = Vec::with_capacity(channels as usize);
        let mut input_buffers = Vec::with_capacity(channels as usize);
        let mut output_buffers = Vec::with_capacity(channels as usize);
        let mut agc_states = Vec::with_capacity(channels as usize);
        let mut eq_filters = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            denoisers.push(nnnoiseless::DenoiseState::new());
            input_buffers.push(Vec::with_capacity(960));
            output_buffers.push(Vec::with_capacity(960));
            agc_states.push(AgcState {
                current_gain: 1.0,
                current_rms: 0.0,
            });
            // 初始化均衡器滤波器 (5 个频段)
            eq_filters.push(vec![BiquadFilter::new(); 5]);
        }

        Self {
            settings,
            sample_rate: sample_rate as f32,
            hpf_prev_x: vec![0.0; channels as usize],
            hpf_prev_y: vec![0.0; channels as usize],
            denoisers,
            input_buffers,
            output_buffers,
            agc_states,
            eq_filters,
        }
    }

    pub fn update_settings(&mut self, settings: DspSettings) {
        // 更新均衡器滤波器系数
        self.update_eq_coefficients();
        self.settings = settings;
    }

    /// 更新均衡器滤波器系数
    fn update_eq_coefficients(&mut self) {
        let channels = self.eq_filters.len();
        for ch in 0..channels {
            for (i, filter) in self.eq_filters[ch].iter_mut().enumerate() {
                if let Some(band) = self.settings.vocal_enhancement.eq_bands.get(i) {
                    // 计算滤波器系数
                    let w0 = 2.0 * std::f32::consts::PI * band.center_freq / self.sample_rate;
                    let cos_w0 = w0.cos();
                    let sin_w0 = w0.sin();
                    let alpha = sin_w0 / (2.0 * band.q_factor);
                    let gain_linear = 10.0_f32.powf(band.gain_db / 20.0);

                    // Peak filter (用于中频增强)
                    let b0 = 1.0 + alpha * gain_linear;
                    let b1 = -2.0 * cos_w0;
                    let b2 = 1.0 - alpha * gain_linear;
                    let a0 = 1.0 + alpha / gain_linear;
                    let a1 = -2.0 * cos_w0;
                    let a2 = 1.0 - alpha / gain_linear;

                    filter.b0 = b0 / a0;
                    filter.b1 = b1 / a0;
                    filter.b2 = b2 / a0;
                    filter.a1 = a1 / a0;
                    filter.a2 = a2 / a0;
                }
            }
        }
    }

    /// 处理一帧音频数据 (交织采样 f32)
    pub fn process_frame(&mut self, data: &mut [f32]) {
        let channels = self.hpf_prev_x.len();
        if data.is_empty() {
            return;
        }

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

        // --- 阶段 5: 自动增益控制 (AGC) ---
        if self.settings.vocal_enhancement.enable_agc {
            let target_rms = self.settings.vocal_enhancement.agc_target_rms;
            let attack_ms = self.settings.vocal_enhancement.agc_attack_ms;
            let release_ms = self.settings.vocal_enhancement.agc_release_ms;
            let max_gain_db = self.settings.vocal_enhancement.agc_max_gain_db;

            // 计算当前帧的 RMS
            let frame_rms = (data.iter().map(|s| s * s).sum::<f32>() / data.len() as f32).sqrt();

            // 更新每个通道的 AGC 状态
            for ch in 0..channels {
                let agc = &mut self.agc_states[ch];

                // 平滑 RMS 估计
                let attack_factor = 1.0 - (1.0 / (attack_ms / 10.0 + 1.0));
                let release_factor = 1.0 - (1.0 / (release_ms / 10.0 + 1.0));

                if frame_rms > agc.current_rms {
                    // 攻击阶段
                    agc.current_rms =
                        agc.current_rms + attack_factor * (frame_rms - agc.current_rms);
                } else {
                    // 释放阶段
                    agc.current_rms =
                        agc.current_rms + release_factor * (frame_rms - agc.current_rms);
                }

                // 计算所需增益
                let mut desired_gain = if agc.current_rms > 0.001 {
                    target_rms / agc.current_rms
                } else {
                    1.0
                };

                // 限制最大增益
                let max_gain_linear = 10.0_f32.powf(max_gain_db / 20.0);
                desired_gain = desired_gain.min(max_gain_linear);

                // 平滑增益变化
                let gain_change_factor = 0.1;
                agc.current_gain =
                    agc.current_gain + gain_change_factor * (desired_gain - agc.current_gain);

                // 应用增益
                for i in (ch..data.len()).step_by(channels) {
                    data[i] *= agc.current_gain;
                }
            }
        }

        // --- 阶段 6: 均衡器 (EQ) ---
        if self.settings.vocal_enhancement.enable_eq {
            for i in 0..data.len() {
                let ch = i % channels;
                let mut sample = data[i];

                // 通过每个均衡器频段
                for filter in self.eq_filters[ch].iter_mut() {
                    let y = filter.b0 * sample
                        + filter.b1 * filter.prev_x1
                        + filter.b2 * filter.prev_x2
                        - filter.a1 * filter.prev_y1
                        - filter.a2 * filter.prev_y2;
                    filter.prev_x2 = filter.prev_x1;
                    filter.prev_x1 = sample;
                    filter.prev_y2 = filter.prev_y1;
                    filter.prev_y1 = y;
                    sample = y;
                }

                data[i] = sample;
            }
        }

        // --- 阶段 7: 限幅器 (Limiter) ---
        if self.settings.vocal_enhancement.enable_limiter {
            let threshold_db = self.settings.vocal_enhancement.limiter_threshold_db;
            let threshold_linear = 10.0_f32.powf(threshold_db / 20.0);

            for i in 0..data.len() {
                let sample = data[i];
                let abs_sample = sample.abs();

                // 简单的硬限幅
                if abs_sample > threshold_linear {
                    let sign = sample.signum();
                    data[i] = sign * threshold_linear;
                }
            }
        }
    }
}

impl BiquadFilter {
    fn new() -> Self {
        Self {
            prev_x1: 0.0,
            prev_x2: 0.0,
            prev_y1: 0.0,
            prev_y2: 0.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }
}
