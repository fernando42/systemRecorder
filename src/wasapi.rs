//! WASAPI 绑定层:设备枚举、会话枚举、捕获(后续里程碑)。
//!
//! ## COM apartment 策略
//! winit/eframe 在 Windows 上会把主线程放进 STA。WASAPI 的一些接口
//! (特别是 `ActivateAudioInterfaceAsync`)需要 MTA。所以本模块**永远不在
//! 调用者所在的线程直接做 WASAPI 操作**,而是把每次操作 dispatch 到一个
//! 专用的 MTA 工作线程,在那儿 CoInitialize 一次、跑闭包、返回结果。

pub mod capture;
pub mod devices;
pub mod sessions;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::fs;

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// 获取应用数据目录。
/// Windows 上使用 `%APPDATA%/SystemRecorder`，确保配置和日志不污染工作目录。
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("SystemRecorder");
    fs::create_dir_all(&dir).ok();
    dir
}

/// 获取日志文件路径
pub fn log_path() -> PathBuf {
    app_data_dir().join("system-recorder.log")
}

/// 获取配置文件路径
pub fn config_path() -> PathBuf {
    app_data_dir().join("config.json")
}

/// 在 MTA 工作线程里跑一个闭包。返回闭包的结果。
///
/// 闭包运行在一个**已经 CoInitializeEx(MTA)** 的线程上,所以内部可以直接
/// 调用任何 WASAPI/COM API,不用自己管 apartment。
pub fn run_on_mta<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    type Job = Box<dyn FnOnce() + Send>;
    static SENDER: OnceLock<Sender<Job>> = OnceLock::new();

    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        thread::Builder::new()
            .name("wasapi-mta".to_string())
            .spawn(move || {
                // SAFETY: 单次初始化,线程结束时 CoUninitialize。
                unsafe {
                    let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                    if hr.is_err() {
                        log::error!("CoInitializeEx 失败: {hr:?}");
                        return;
                    }
                }
                while let Ok(job) = rx.recv() {
                    job();
                }
                unsafe { CoUninitialize() };
            })
            .expect("spawn wasapi-mta thread");
        tx
    });

    let (result_tx, result_rx) = channel();
    let job: Job = Box::new(move || {
        let r = f();
        let _ = result_tx.send(r);
    });
    sender.send(job).expect("wasapi-mta thread alive");
    result_rx
        .recv()
        .expect("wasapi-mta thread returned no result")
}

#[derive(Debug, thiserror::Error)]
pub enum WasapiError {
    #[error("Windows API 调用失败: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("字符串解码失败")]
    BadString,
    #[error("不支持的音频格式: {0}")]
    UnsupportedFormat(String),
    #[error("WAV 写入失败: {0}")]
    Wav(#[from] hound::Error),
    #[error("捕获线程 panic")]
    ThreadPanic,
}

pub type Result<T> = std::result::Result<T, WasapiError>;

/// 是否支持按进程 loopback 捕获(需要 Windows 10 build 20348+)。
pub fn supports_process_loopback() -> bool {
    let v = windows_version::OsVersion::current();
    // Windows 10 / 11 都是 major=10。区分在 build 号。
    v.major >= 10 && v.build >= 20348
}

pub fn os_version_string() -> String {
    let v = windows_version::OsVersion::current();
    format!(
        "Windows {}.{}.{} (build {})",
        v.major, v.minor, v.build, v.build
    )
}
