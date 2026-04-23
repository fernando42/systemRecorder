// system-recorder: Windows 跨源录音工具 (mic / system loopback / per-process loopback)
//
// 当前进度: M1 - 设备与会话枚举

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod wasapi;

#[cfg(windows)]
fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 720.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "System Recorder",
        options,
        Box::new(|cc| Ok(Box::new(app::RecorderApp::new(cc)))),
    )
}

#[cfg(not(windows))]
fn main() {
    eprintln!("system-recorder 只支持 Windows。请用 `cargo build --target x86_64-pc-windows-gnu` 交叉编译。");
    std::process::exit(1);
}
