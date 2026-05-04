//! egui 前端。M1 只做只读展示:
//! - 顶部条:OS 版本、是否支持 per-process loopback
//! - 三列:输入设备、输出设备、音频会话
//! - 右上角"刷新"按钮

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use std::fs;

use eframe::CreationContext;
use egui::{Color32, FontData, FontDefinitions, FontFamily, RichText};

use crate::wasapi::{
    self,
    capture::{CaptureSource, CaptureStats, WasapiCapture, NamingMode, SequenceType, generate_output_filename, default_output_path},
    devices::{EndpointDevice, EndpointFlow},
    sessions::{AudioSession, SessionState},
};
use crate::dsp::DspSettings;

#[derive(Serialize, Deserialize, Clone)]
struct NamingRule {
    name: String,
    mode: NamingMode,
    current_sequence: usize,
}

#[derive(Serialize, Deserialize)]
struct AppConfig {
    last_dir: Option<String>,
    naming_rules: Vec<NamingRule>,
    selected_rule_idx: usize,
    dsp_settings: DspSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_dir: None,
            naming_rules: vec![NamingRule {
                name: "默认 (来源+时间戳)".to_string(),
                mode: NamingMode::Timestamped,
                current_sequence: 0,
            }],
            selected_rule_idx: 0,
            dsp_settings: DspSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Mic,
    SystemLoopback,
    PerProcess,
}

impl SourceKind {
    fn prefix(self) -> &'static str {
        match self {
            SourceKind::Mic => "mic",
            SourceKind::SystemLoopback => "loopback",
            SourceKind::PerProcess => "app",
        }
    }
}

pub struct RecorderApp {
    os_version: String,
    supports_per_process: bool,
    input_devices: Result<Vec<EndpointDevice>, String>,
    output_devices: Result<Vec<EndpointDevice>, String>,
    sessions: Result<Vec<AudioSession>, String>,

    // 命名规则配置
    config: AppConfig,
    show_naming_window: bool,

    // 音效增强设置
    dsp_settings: Arc<RwLock<DspSettings>>,
    show_dsp_window: bool,

    // 录制状态
    source_kind: SourceKind,
    selected_input_id: Option<String>,
    selected_output_id: Option<String>, // 用于 loopback
    selected_pid: Option<u32>,          // 用于 per-process
    output_path_buf: String,
    recording: RecordingState,
}

enum RecordingState {
    Idle,
    Active {
        capture: WasapiCapture,
        started_at: Instant,
    },
    LastResult {
        path: PathBuf,
        outcome: Result<CaptureStats, String>,
    },
}

impl RecorderApp {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);

        let config_path = "config.json";
        let config = fs::read_to_string(config_path)
            .ok()
            .and_then(|content| serde_json::from_str::<AppConfig>(&content).ok())
            .unwrap_or_default();

        let mut app = Self {
            os_version: wasapi::os_version_string(),
            supports_per_process: wasapi::supports_process_loopback(),
            input_devices: Ok(vec![]),
            output_devices: Ok(vec![]),
            sessions: Ok(vec![]),
            dsp_settings: Arc::new(RwLock::new(config.dsp_settings.clone())),
            config,
            show_naming_window: false,
            show_dsp_window: false,
            source_kind: SourceKind::Mic,
            selected_input_id: None,
            selected_output_id: None,
            selected_pid: None,
            output_path_buf: "".to_string(), // Initialized below
            recording: RecordingState::Idle,
        };

        // Correctly initialize output_path_buf from config after 'app' is created
        app.output_path_buf = app.config.last_dir.clone().unwrap_or_else(|| ".".to_string());
        
        app.refresh();
        // 默认选系统默认的输入/输出
        if let Ok(list) = &app.input_devices
            && let Some(def) = list.iter().find(|d| d.is_default).or(list.first())
        {
            app.selected_input_id = Some(def.id.clone());
        }
        if let Ok(list) = &app.output_devices
            && let Some(def) = list.iter().find(|d| d.is_default).or(list.first())
        {
            app.selected_output_id = Some(def.id.clone());
        }
        app
    }

    fn refresh(&mut self) {
        self.input_devices =
            wasapi::devices::list_input_devices().map_err(|e| e.to_string());
        self.output_devices =
            wasapi::devices::list_output_devices().map_err(|e| e.to_string());
        self.sessions = wasapi::sessions::list_audio_sessions().map_err(|e| e.to_string());
    }

    /// 若录制线程自发退出(例如初始化错误)把结果吸收到 LastResult。
    fn poll_recording(&mut self) {
        if let RecordingState::Active { capture, .. } = &mut self.recording
            && let Some(result) = capture.try_take_result()
        {
            let path = capture.output_path().to_path_buf();
            let outcome = result.map_err(|e| e.to_string());
            self.recording = RecordingState::LastResult { path, outcome };
        }
    }

    fn dsp_settings_ui(&mut self, ui: &mut egui::Ui) {
        // Removed the header label as it's now in the window title
        let mut settings = self.dsp_settings.write().unwrap();
        let mut changed = false;
        
        ui.group(|ui| {
            ui.label("高通滤波 (切除低频嗡嗡声)");
            if ui.checkbox(&mut settings.enable_hpf, "启用").changed() {
                changed = true;
            }
            if settings.enable_hpf {
                if ui.add(egui::Slider::new(&mut settings.hpf_cutoff, 20.0..=500.0).text("截止频率 (Hz)")).changed() {
                    changed = true;
                }
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.label("噪声门 (静音背景噪音)");
            if ui.checkbox(&mut settings.enable_gate, "启用").changed() {
                changed = true;
            }
            if settings.enable_gate {
                if ui.add(egui::Slider::new(&mut settings.gate_threshold, 0.0..=0.1).text("阈值")).changed() {
                    changed = true;
                }
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.label("简单压缩器 (防止爆音/增强人声)");
            if ui.checkbox(&mut settings.enable_compressor, "启用").changed() {
                changed = true;
            }
            if settings.enable_compressor {
                if ui.add(egui::Slider::new(&mut settings.comp_threshold, 0.1..=1.0).text("阈值")).changed() {
                    changed = true;
                }
                if ui.add(egui::Slider::new(&mut settings.comp_ratio, 1.0..=20.0).text("压缩比")).changed() {
                    changed = true;
                }
            }
        });

        // 同步回 config 并持久化
        if changed {
            self.config.dsp_settings = settings.clone();
            if let Ok(json) = serde_json::to_string(&self.config) {
                let _ = fs::write("config.json", json);
            }
        }
    }

    fn dsp_window_ui(&mut self, ui: &mut egui::Ui) {
        let mut is_open = self.show_dsp_window;
        egui::Window::new("音效增强设置")
            .open(&mut is_open)
            .resizable(false)
            .default_width(300.0)
            .show(ui.ctx(), |ui| {
                self.dsp_settings_ui(ui);
            });
        self.show_dsp_window = is_open;
    }

    fn naming_window_ui(&mut self, ui: &mut egui::Ui) {
        let mut is_open = self.show_naming_window;
        egui::Window::new("录音命名规则")
            .open(&mut is_open)
            .resizable(false)
            .default_width(400.0)
            .show(ui.ctx(), |ui| {
                ui.label("选择当前使用的规则:");
                
                let mut changed = false;
                ui.horizontal_wrapped(|ui| {
                    for (i, rule) in self.config.naming_rules.iter().enumerate() {
                        if ui.radio_value(&mut self.config.selected_rule_idx, i, &rule.name).clicked() {
                            changed = true;
                        }
                    }
                });

                ui.separator();

                if let Some(rule) = self.config.naming_rules.get_mut(self.config.selected_rule_idx) {
                    ui.label(RichText::new("规则设置").strong());
                    
                    ui.horizontal(|ui| {
                        ui.label("规则名称:");
                        if ui.text_edit_singleline(&mut rule.name).changed() {
                            changed = true;
                        }
                    });

                    ui.separator();

                    match &mut rule.mode {
                        NamingMode::Timestamped => {
                            ui.label("模式: 来源 + 时间戳 (例如: mic-20231027-103000.wav)");
                            if ui.button("更改模式").clicked() {
                                // This is a bit tricky in egui, we'll use a temporary state or just change it here
                                // For simplicity, let's provide buttons to switch modes
                            }
                            ui.horizontal(|ui| {
                                if ui.button("改为固定名称").clicked() {
                                    rule.mode = NamingMode::Fixed("recording".to_string());
                                    changed = true;
                                }
                                if ui.button("改为自增序号").clicked() {
                                    rule.mode = NamingMode::AutoIncrement { 
                                        prefix: "".to_string(), 
                                        sequence: SequenceType::Numeric 
                                    };
                                    changed = true;
                                }
                            });
                        }
                        NamingMode::Fixed(name) => {
                            ui.label("模式: 固定名称");
                            ui.horizontal(|ui| {
                                ui.label("文件名:");
                                if ui.text_edit_singleline(name).changed() {
                                    changed = true;
                                }
                            });
                            if ui.button("改为时间戳").clicked() {
                                rule.mode = NamingMode::Timestamped;
                                changed = true;
                            }
                        }
                        NamingMode::AutoIncrement { prefix, sequence } => {
                            ui.label("模式: 自动增量");
                            ui.horizontal(|ui| {
                                ui.label("前缀:");
                                if ui.text_edit_singleline(prefix).changed() {
                                    changed = true;
                                }
                            });
                            
                            ui.horizontal(|ui| {
                                ui.label("序号类型:");
                            if ui.radio_value(sequence, SequenceType::Numeric, "数字 (1,2,3)").clicked() {
                                changed = true;
                            }
                            if ui.radio_value(sequence, SequenceType::AlphabeticLower, "小写字母 (a,b,c)").clicked() {
                                changed = true;
                            }
                            if ui.radio_value(sequence, SequenceType::AlphabeticUpper, "大写字母 (A,B,C)").clicked() {
                                changed = true;
                            }
                            });

                            if ui.button("改为时间戳").clicked() {
                                rule.mode = NamingMode::Timestamped;
                                changed = true;
                            }
                        }
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("➕ 添加规则").clicked() {
                        self.config.naming_rules.push(NamingRule {
                            name: format!("新规则 {}", self.config.naming_rules.len()),
                            mode: NamingMode::Timestamped,
                            current_sequence: 0,
                        });
                        self.config.selected_rule_idx = self.config.naming_rules.len() - 1;
                        changed = true;
                    }

                    if self.config.naming_rules.len() > 1 {
                        if ui.button("🗑 删除选中").clicked() {
                            self.config.naming_rules.remove(self.config.selected_rule_idx);
                            if self.config.selected_rule_idx >= self.config.naming_rules.len() {
                                self.config.selected_rule_idx = self.config.naming_rules.len() - 1;
                            }
                            changed = true;
                        }
                    }
                });

                if changed {
                    if let Ok(json) = serde_json::to_string(&self.config) {
                        let _ = fs::write("config.json", json);
                    }
                }
            });
        self.show_naming_window = is_open;
    }

    fn record_panel_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);

        let idle = matches!(
            self.recording,
            RecordingState::Idle | RecordingState::LastResult { .. }
        );

        ui.horizontal(|ui| {
            ui.label(RichText::new("录制").strong());
            ui.separator();
            ui.add_enabled_ui(idle, |ui| {
                ui.label("源:");
                let prev = self.source_kind;
                ui.radio_value(&mut self.source_kind, SourceKind::Mic, "麦克风");
                ui.radio_value(
                    &mut self.source_kind,
                    SourceKind::SystemLoopback,
                    "系统 loopback",
                );
                // per-process 需要 Win10 build 20348+;老版本不可选
                ui.add_enabled_ui(self.supports_per_process, |ui| {
                    ui.radio_value(
                        &mut self.source_kind,
                        SourceKind::PerProcess,
                        "特定应用",
                    )
                    .on_disabled_hover_text(
                        "需要 Windows 10 build 20348+ / Windows 11",
                    );
                });
                // 切换源类型时,默认文件名前缀跟着换(除非用户已改过路径)
                if prev != self.source_kind
                    && self.output_path_buf
                        == default_output_path(prev.prefix()).display().to_string()
                {
                    self.output_path_buf = default_output_path(self.source_kind.prefix())
                        .display()
                        .to_string();
                }
                ui.separator();

                // 按源类型切换选择控件
                match self.source_kind {
                    SourceKind::Mic => {
                        ui.label("设备:");
                        let inputs =
                            self.input_devices.as_ref().ok().cloned().unwrap_or_default();
                        device_combo(
                            ui,
                            "mic_picker",
                            &inputs,
                            &mut self.selected_input_id,
                            "<选择麦克风>",
                        );
                    }
                    SourceKind::SystemLoopback => {
                        ui.label("设备:");
                        let outputs = self
                            .output_devices
                            .as_ref()
                            .ok()
                            .cloned()
                            .unwrap_or_default();
                        device_combo(
                            ui,
                            "loopback_picker",
                            &outputs,
                            &mut self.selected_output_id,
                            "<选择输出端点>",
                        );
                    }
                    SourceKind::PerProcess => {
                        ui.label("应用:");
                        let sessions =
                            self.sessions.as_ref().ok().cloned().unwrap_or_default();
                        session_combo(
                            ui,
                            "process_picker",
                            &sessions,
                            &mut self.selected_pid,
                        );
                    }
                }

                ui.separator();
                ui.label("输出:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.output_path_buf)
                            .desired_width(240.0),
                    );
                    if ui.button("浏览...").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title("选择保存目录")
                            .pick_folder() {
                            self.output_path_buf = path.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("重置路径").clicked() {
                        self.output_path_buf = ".".to_string();
                    }
                    if ui.button("命名规则...").clicked() {
                        self.show_naming_window = true;
                    }
                });
            });
        });

        // UI 阶段只读状态、收集一个 Intent;所有状态转换在 UI 结束后统一做,
        // 避免在 match 借用期间写 self.recording。
        enum Intent {
            None,
            Start,
            Stop,
        }
        let mut intent = Intent::None;

        // 待启动的源描述(含选中的设备/PID)。None 表示当前选择不完整。
        let pending_source: Option<CaptureSource> = match self.source_kind {
            SourceKind::Mic => self
                .selected_input_id
                .clone()
                .map(|id| CaptureSource::Mic { device_id: id }),
            SourceKind::SystemLoopback => self
                .selected_output_id
                .clone()
                .map(|id| CaptureSource::SystemLoopback { device_id: id }),
            SourceKind::PerProcess => self.selected_pid.map(|pid| CaptureSource::PerProcess {
                pid,
                include_tree: true,
            }),
        };

        ui.horizontal(|ui| {
            let can_start = pending_source.is_some()
                && !self.output_path_buf.trim().is_empty();

            match &self.recording {
                RecordingState::Idle | RecordingState::LastResult { .. } => {
                    let btn = egui::Button::new(
                        RichText::new("● 开始录制").color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(200, 60, 60));
                    if ui.add_enabled(can_start, btn).clicked() {
                        intent = Intent::Start;
                    }
                }
                RecordingState::Active { started_at, .. } => {
                    let elapsed = started_at.elapsed();
                    let btn = egui::Button::new(
                        RichText::new("■ 停止").color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(80, 130, 200));
                    if ui.add(btn).clicked() {
                        intent = Intent::Stop;
                    }
                    ui.label(format!(
                        "● 正在录制  {:02}:{:02}.{}",
                        elapsed.as_secs() / 60,
                        elapsed.as_secs() % 60,
                        elapsed.subsec_millis() / 100,
                    ));
                }
            }

            match &self.recording {
                RecordingState::LastResult { path, outcome } => match outcome {
                    Ok(stats) => {
                        ui.colored_label(
                            Color32::from_rgb(80, 200, 120),
                            format!(
                                "✓ 已保存 {} ({} Hz / {} ch / {} 帧)",
                                path.display(),
                                stats.sample_rate,
                                stats.channels,
                                stats.frames,
                            ),
                        );
                    }
                    Err(e) => {
                        ui.colored_label(Color32::LIGHT_RED, format!("录制失败: {e}"));
                    }
                },
                RecordingState::Idle => {
                    ui.label("就绪");
                }
                RecordingState::Active { .. } => {}
            }
        });

        match intent {
            Intent::None => {}
                Intent::Start => {
                    if let Some(source) = pending_source {
                        let dir_str = self.output_path_buf.trim().to_string();
                        let dir = PathBuf::from(&dir_str);
                        
                        // Use the selected naming rule from config
                        let rule_idx = self.config.selected_rule_idx;
                        let rule = &self.config.naming_rules[rule_idx];
                        
                        // Handle sequence increment for AutoIncrement mode
                        let override_idx = match &rule.mode {
                            NamingMode::AutoIncrement { .. } => Some(rule.current_sequence + 1),
                            _ => None,
                        };

                        let full_path = generate_output_filename(
                            self.source_kind.prefix(), 
                            &rule.mode, 
                            &dir,
                            override_idx
                        );

                        // Update sequence and persist config
                        if let NamingMode::AutoIncrement { .. } = &rule.mode {
                            self.config.naming_rules[rule_idx].current_sequence += 1;
                        }

                        self.config.last_dir = Some(dir_str);
                        if let Ok(json) = serde_json::to_string(&self.config) {
                            let _ = fs::write("config.json", json);
                        }
        
                        let cap = WasapiCapture::start(source, full_path, self.dsp_settings.clone());
                        self.recording = RecordingState::Active {
                            capture: cap,
                            started_at: Instant::now(),
                        };
                    }
                }
            Intent::Stop => {
                let prev = std::mem::replace(&mut self.recording, RecordingState::Idle);
                if let RecordingState::Active { capture, .. } = prev {
                    let path = capture.output_path().to_path_buf();
                    let outcome = capture.stop().map_err(|e| e.to_string());
                    self.recording = RecordingState::LastResult { path, outcome };
                }
            }
        }
        ui.add_space(4.0);
    }
}

impl eframe::App for RecorderApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_recording();

        if self.show_naming_window {
            self.naming_window_ui(ui);
        }
        // 录制中每秒刷 4 次,以便计时器和状态实时更新
        if matches!(self.recording, RecordingState::Active { .. }) {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        egui::Panel::top("header").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("System Recorder");
                ui.separator();
                ui.label(&self.os_version);
                ui.separator();
                let (txt, color) = if self.supports_per_process {
                    ("Per-App 捕获: 可用", Color32::from_rgb(80, 200, 120))
                } else {
                    (
                        "Per-App 捕获: 不可用 (需 Win10 build 20348+)",
                        Color32::from_rgb(220, 160, 80),
                    )
                };
                ui.label(RichText::new(txt).color(color));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("刷新").clicked() {
                        self.refresh();
                    }
                });
            });
        });

        egui::Panel::bottom("record_panel").show_inside(ui, |ui| {
            self.record_panel_ui(ui);
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("⚙ 音效设置").clicked() {
                    self.show_dsp_window = !self.show_dsp_window;
                }
            });

            if self.show_dsp_window {
                self.dsp_window_ui(ui);
            }

            ui.columns(3, |cols| {
                device_column(
                    &mut cols[0],
                    "输入设备 (麦克风)",
                    "scroll_inputs",
                    &self.input_devices,
                );
                device_column(
                    &mut cols[1],
                    "输出设备 (系统 Loopback)",
                    "scroll_outputs",
                    &self.output_devices,
                );
                session_column(
                    &mut cols[2],
                    "音频会话 (Per-App)",
                    "scroll_sessions",
                    &self.sessions,
                    self.supports_per_process,
                );
            });
        });
    }
}

fn device_column(
    ui: &mut egui::Ui,
    title: &str,
    scroll_id: &str,
    devices: &Result<Vec<EndpointDevice>, String>,
) {
    ui.label(RichText::new(title).heading());
    ui.separator();
    match devices {
        Err(e) => {
            ui.colored_label(Color32::LIGHT_RED, format!("枚举失败: {e}"));
        }
        Ok(list) if list.is_empty() => {
            ui.label("(无)");
        }
        Ok(list) => {
            egui::ScrollArea::vertical().id_salt(scroll_id).show(ui, |ui| {
                for d in list {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(&d.friendly_name);
                            if d.is_default {
                                ui.label(
                                    RichText::new("默认")
                                        .small()
                                        .color(Color32::from_rgb(100, 180, 255)),
                                );
                            }
                        });
                        let flow = match d.flow {
                            EndpointFlow::Render => "render",
                            EndpointFlow::Capture => "capture",
                        };
                        ui.label(
                            RichText::new(format!("{flow} · {}", d.id))
                                .small()
                                .color(Color32::GRAY),
                        );
                    });
                }
            });
        }
    }
}

fn session_column(
    ui: &mut egui::Ui,
    title: &str,
    scroll_id: &str,
    sessions: &Result<Vec<AudioSession>, String>,
    enabled: bool,
) {
    ui.label(RichText::new(title).heading());
    if !enabled {
        ui.colored_label(
            Color32::from_rgb(220, 160, 80),
            "当前 Windows 版本不支持按进程捕获,仅作展示。",
        );
    }
    ui.separator();
    match sessions {
        Err(e) => {
            ui.colored_label(Color32::LIGHT_RED, format!("枚举失败: {e}"));
        }
        Ok(list) if list.is_empty() => {
            ui.label("(无正在使用的音频会话)");
        }
        Ok(list) => {
            egui::ScrollArea::vertical().id_salt(scroll_id).show(ui, |ui| {
                for s in list {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(s.best_label());
                            ui.label(state_tag(s.state));
                        });
                        let subtitle = ui.label(
                            RichText::new(format!("PID {}", s.pid))
                                .small()
                                .color(Color32::GRAY),
                        );
                        // 完整路径放 hover 提示,避免长路径撑爆窄列
                        if !s.exe_path.is_empty() {
                            subtitle.on_hover_text(&s.exe_path);
                        }
                    });
                }
            });
        }
    }
}

fn session_combo(
    ui: &mut egui::Ui,
    id_salt: &str,
    sessions: &[AudioSession],
    selected: &mut Option<u32>,
) {
    // 只提供可录的会话(有 PID 且不是系统提示音;系统提示音没有 PID 入口,
    // 若以后想录它得走 SystemLoopback)。
    let usable: Vec<&AudioSession> = sessions
        .iter()
        .filter(|s| s.pid != 0 && !s.is_system_sounds)
        .collect();

    let current_label = selected
        .and_then(|pid| usable.iter().find(|s| s.pid == pid))
        .map(|s| format!("{} (PID {})", s.best_label(), s.pid))
        .unwrap_or_else(|| "<选择应用>".to_string());

    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current_label)
        .width(260.0)
        .show_ui(ui, |ui| {
            if usable.is_empty() {
                ui.label(
                    RichText::new("(当前无可录的进程音频会话)")
                        .color(Color32::GRAY),
                );
            }
            for s in usable {
                ui.selectable_value(
                    selected,
                    Some(s.pid),
                    format!("{} (PID {})", s.best_label(), s.pid),
                );
            }
        });
}

fn device_combo(
    ui: &mut egui::Ui,
    id_salt: &str,
    devices: &[EndpointDevice],
    selected: &mut Option<String>,
    placeholder: &str,
) {
    let current_label = selected
        .as_ref()
        .and_then(|id| devices.iter().find(|d| &d.id == id))
        .map(|d| d.friendly_name.clone())
        .unwrap_or_else(|| placeholder.to_string());

    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current_label)
        .width(260.0)
        .show_ui(ui, |ui| {
            for d in devices {
                ui.selectable_value(
                    selected,
                    Some(d.id.clone()),
                    if d.is_default {
                        format!("★ {}", d.friendly_name)
                    } else {
                        d.friendly_name.clone()
                    },
                );
            }
        });
}

/// 给 egui 注册 Windows 自带的中文字体,否则 CJK 字符全变方块。
///
/// 优先微软雅黑 (msyh.ttc),退路 中易宋体 (simsun.ttc)。两者在 Win10+ 基础
/// 安装里都有,不用外带字体资源。加载失败只记日志、不报错。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑 Regular
        r"C:\Windows\Fonts\simsun.ttc", // 中易宋体
        r"C:\Windows\Fonts\simhei.ttf", // 中易黑体
    ];

    let Some((path, bytes)) = CANDIDATES.iter().find_map(|p| {
        std::fs::read(p).ok().map(|b| (*p, b))
    }) else {
        log::warn!("未找到任何系统中文字体,CJK 字符将显示为方块");
        return;
    };
    log::info!("加载中文字体: {path} ({} bytes)", bytes.len());

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(FontData::from_owned(bytes)), // TTC: index=0 默认即 Regular face
    );
    // 中文作为正文、等宽族的 **fallback**:ASCII 仍由默认字体渲染,遇到
    // 无显示字形才回退到中文字体。
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn state_tag(state: SessionState) -> RichText {
    match state {
        SessionState::Active => {
            RichText::new("active").small().color(Color32::from_rgb(80, 200, 120))
        }
        SessionState::Inactive => {
            RichText::new("inactive").small().color(Color32::GRAY)
        }
        SessionState::Expired => {
            RichText::new("expired").small().color(Color32::from_rgb(180, 100, 100))
        }
    }
}