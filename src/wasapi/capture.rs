//! WASAPI 捕获到 WAV。
//!
//! 支持三种音源:
//! - [`CaptureSource::Mic`][]: 输入端点(麦克风/线路),事件驱动。
//! - [`CaptureSource::SystemLoopback`][]: 输出端点的 loopback,轮询驱动
//!   (微软官方文档确认 loopback + event callback 组合对 render endpoint 不可靠)。
//! - [`CaptureSource::PerProcess`][]: 特定进程的 loopback,通过
//!   `ActivateAudioInterfaceAsync` + `PROCESS_LOOPBACK` 激活虚拟设备,
//!   事件驱动(此路径下 event callback 可以正常工作,见 ApplicationLoopback 示例)。
//!
//! 设计:
//! - 每次录音起一个专用 MTA 线程,和 UI/枚举线程彻底隔离。
//! - 共享模式,格式由源决定:端点源用设备原生 mix format,per-process 源固定
//!   PCM 16-bit 48kHz stereo(进程 loopback API 只支持固定的几组格式)。
//! - 支持 IEEE float32 和 PCM int16。

use std::mem::{ManuallyDrop, size_of};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use windows::Win32::Foundation::{CloseHandle, HANDLE, SYSTEMTIME, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, implement};

use super::{Result, WasapiError};

// 取自 mmreg.h,数值级 API 常年稳定。避免再开 windows crate 的 Multimedia /
// KernelStreaming 两个 feature。
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: GUID =
    GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

const REFTIMES_PER_MS: i64 = 10_000; // 100-ns 单位
const BUFFER_DURATION_MS: i64 = 200;
const LOOPBACK_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ACTIVATE_TIMEOUT_MS: u32 = 5_000;

/// "crash-proof" 诊断写入:per-process loopback 的 COM 路径在出错时容易触发
/// SEH 异常直接终结进程,env_logger 的缓冲区可能丢;这里每条日志立刻追加写
/// `system-recorder.log` 并 flush,确保哪怕下一行代码就 AV,这条也落盘了。
macro_rules! diag {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        log::info!("{msg}");
        diag_to_file(&msg);
    }};
}

fn diag_to_file(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("system-recorder.log")
    {
        let _ = writeln!(f, "[{:?}] {msg}", std::thread::current().id());
        let _ = f.flush();
    }
}

#[derive(Debug, Clone)]
pub enum CaptureSource {
    /// 输入端点(麦克风/线路输入)
    Mic { device_id: String },
    /// 输出端点的 loopback(系统混音)
    SystemLoopback { device_id: String },
    /// 特定进程的 loopback。`include_tree=true` 包含该 PID 的子进程。
    PerProcess { pid: u32, include_tree: bool },
}

impl CaptureSource {
    fn log_tag(&self) -> &'static str {
        match self {
            Self::Mic { .. } => "mic",
            Self::SystemLoopback { .. } => "loopback",
            Self::PerProcess { .. } => "app",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PcmKind {
    Float32,
    Int16,
}

#[derive(Debug, Clone, Copy)]
struct FormatInfo {
    kind: PcmKind,
    channels: u16,
    sample_rate: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureStats {
    pub frames: u64,
    pub channels: u16,
    pub sample_rate: u32,
}

/// 正在进行的一次 WASAPI 录制。构造即开始;`stop()` 等线程退出并收尾 WAV。
pub struct WasapiCapture {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<CaptureStats>>>,
    output_path: PathBuf,
}

impl WasapiCapture {
    /// 立即启动捕获线程,开始向 `output_path` 写 WAV。
    pub fn start(source: CaptureSource, output_path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = stop.clone();
        let path_c = output_path.clone();
        let thread_name = format!("wasapi-{}-capture", source.log_tag());

        let join = thread::Builder::new()
            .name(thread_name)
            .spawn(move || -> Result<CaptureStats> {
                diag!("capture thread start: source={:?} path={}", source, path_c.display());
                // SAFETY: 一次 init,线程结束前一次 uninit,中间所有 WASAPI 调用合法。
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
                // Rust panic 不会带进程下去,但 COM 路径上偶发的 SEH 异常会。
                // 用 catch_unwind 兜住 Rust 侧,SEH 则由外层 diag 日志定位崩溃点。
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_capture(&source, &path_c, &stop_c)
                }));
                unsafe { CoUninitialize() };
                match r {
                    Ok(res) => {
                        diag!("capture thread end: ok={}", res.is_ok());
                        res
                    }
                    Err(_) => {
                        diag!("capture thread: Rust panic caught");
                        Err(WasapiError::ThreadPanic)
                    }
                }
            })
            .expect("spawn wasapi capture thread");

        Self {
            stop,
            join: Some(join),
            output_path,
        }
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// 发停止信号、等待线程、取 stats。
    pub fn stop(mut self) -> Result<CaptureStats> {
        self.stop.store(true, Ordering::SeqCst);
        let h = self.join.take().expect("stop called twice");
        h.join().map_err(|_| WasapiError::ThreadPanic)?
    }

    /// 如果线程已自发终止,取出结果。否则返回 None。
    pub fn try_take_result(&mut self) -> Option<Result<CaptureStats>> {
        if self.join.as_ref()?.is_finished() {
            let h = self.join.take()?;
            Some(h.join().unwrap_or(Err(WasapiError::ThreadPanic)))
        } else {
            None
        }
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// 生成默认输出文件名:`<prefix>-YYYYMMDD-HHMMSS.wav`(当前工作目录)。
pub fn default_output_path(prefix: &str) -> PathBuf {
    let st: SYSTEMTIME = unsafe { GetLocalTime() };
    PathBuf::from(format!(
        "{prefix}-{:04}{:02}{:02}-{:02}{:02}{:02}.wav",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
    ))
}

// ---------- 内部 ----------

/// 持有 WAVEFORMATEX 指针,保证与它的来源绑定的释放语义。
enum FormatHolder {
    /// 来自 `IAudioClient::GetMixFormat`,必须 `CoTaskMemFree`。
    MixFormat(*mut WAVEFORMATEX),
    /// Rust 拥有的栈/堆内存,Drop 时自动释放。
    Owned(Box<WAVEFORMATEX>),
}

impl FormatHolder {
    fn as_ptr(&self) -> *const WAVEFORMATEX {
        match self {
            Self::MixFormat(p) => *p,
            Self::Owned(b) => &**b as *const _,
        }
    }
}

impl Drop for FormatHolder {
    fn drop(&mut self) {
        if let Self::MixFormat(p) = *self {
            unsafe { CoTaskMemFree(Some(p as *const _)) };
        }
    }
}

fn run_capture(
    source: &CaptureSource,
    output_path: &Path,
    stop: &AtomicBool,
) -> Result<CaptureStats> {
    diag!("run_capture enter: source={:?}", source);
    unsafe {
        let (audio_client, format_holder) = match activate_audio_client(source) {
            Ok(v) => v,
            Err(e) => {
                diag!("run_capture: activate_audio_client failed: {e}");
                return Err(e);
            }
        };
        diag!("run_capture: audio_client activated");
        let pformat = format_holder.as_ptr();
        let fmt = inspect_format(pformat)?;
        diag!("run_capture: format inspected -> {:?}", fmt);

        // 流式参数随源类型而异:
        // - Mic / PerProcess: 事件驱动
        // - SystemLoopback: 轮询(MSDN: loopback + event 在 render endpoint 上
        //   不可靠)。PerProcess 走的是独立的虚拟设备,不受此限制。
        let (stream_flags, event) = match source {
            CaptureSource::Mic { .. } => {
                let e = CreateEventW(None, false, false, PCWSTR::null())?;
                (AUDCLNT_STREAMFLAGS_EVENTCALLBACK, Some(e))
            }
            CaptureSource::SystemLoopback { .. } => (AUDCLNT_STREAMFLAGS_LOOPBACK, None),
            CaptureSource::PerProcess { .. } => {
                let e = CreateEventW(None, false, false, PCWSTR::null())?;
                (
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    Some(e),
                )
            }
        };

        diag!("run_capture: before IAudioClient::Initialize (flags={stream_flags:?})");
        let init_hr = audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            BUFFER_DURATION_MS * REFTIMES_PER_MS,
            0,
            pformat,
            None,
        );
        if let Err(e) = &init_hr {
            diag!("run_capture: Initialize failed hr={:#010x} msg={}", e.code().0 as u32, e.message());
        }
        init_hr?;
        diag!("run_capture: Initialize OK");
        if let Some(e) = event {
            audio_client.SetEventHandle(e)?;
            diag!("run_capture: SetEventHandle OK");
        }
        drop(format_holder);

        let capture_client: IAudioCaptureClient = audio_client.GetService()?;

        let spec = WavSpec {
            channels: fmt.channels,
            sample_rate: fmt.sample_rate,
            bits_per_sample: match fmt.kind {
                PcmKind::Float32 => 32,
                PcmKind::Int16 => 16,
            },
            sample_format: match fmt.kind {
                PcmKind::Float32 => SampleFormat::Float,
                PcmKind::Int16 => SampleFormat::Int,
            },
        };
        let mut writer = WavWriter::create(output_path, spec)?;

        audio_client.Start()?;
        log::info!(
            "[{}] 开始录制 {} Hz / {} ch / {:?} → {}",
            source.log_tag(),
            fmt.sample_rate,
            fmt.channels,
            fmt.kind,
            output_path.display()
        );

        let mut frames: u64 = 0;
        while !stop.load(Ordering::SeqCst) {
            match event {
                Some(e) => {
                    if WaitForSingleObject(e, 500) != WAIT_OBJECT_0 {
                        continue;
                    }
                }
                None => {
                    thread::sleep(LOOPBACK_POLL_INTERVAL);
                }
            }

            loop {
                let packet = capture_client.GetNextPacketSize()?;
                if packet == 0 {
                    break;
                }

                let mut pdata: *mut u8 = ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                capture_client.GetBuffer(
                    &mut pdata,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )?;

                if num_frames > 0 {
                    let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                    write_frames(&mut writer, &fmt, pdata, num_frames as usize, silent)?;
                    frames += num_frames as u64;
                }
                capture_client.ReleaseBuffer(num_frames)?;
            }
        }

        audio_client.Stop()?;
        writer.finalize()?;
        if let Some(e) = event {
            let _ = CloseHandle(e);
        }
        log::info!("[{}] 结束,共 {frames} 帧", source.log_tag());

        Ok(CaptureStats {
            frames,
            channels: fmt.channels,
            sample_rate: fmt.sample_rate,
        })
    }
}

/// 按源类型拿到 `IAudioClient` + 对应的 WAVEFORMATEX 所有权。
unsafe fn activate_audio_client(
    source: &CaptureSource,
) -> Result<(IAudioClient, FormatHolder)> {
    unsafe {
        match source {
            CaptureSource::Mic { device_id }
            | CaptureSource::SystemLoopback { device_id } => {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device: IMMDevice = enumerator.GetDevice(&HSTRING::from(device_id.as_str()))?;
                let audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let pformat = audio_client.GetMixFormat()?;
                Ok((audio_client, FormatHolder::MixFormat(pformat)))
            }
            CaptureSource::PerProcess { pid, include_tree } => {
                let audio_client = activate_process_loopback_client(*pid, *include_tree)?;
                diag!("activate_audio_client: per-process client owned");
                match audio_client.GetMixFormat() {
                    Ok(pformat) => {
                        diag!(
                            "activate_audio_client: per-process mix format ptr {:p}",
                            pformat
                        );
                        Ok((audio_client, FormatHolder::MixFormat(pformat)))
                    }
                    Err(e) => {
                        diag!(
                            "activate_audio_client: per-process GetMixFormat failed hr={:#010x} msg={}; fallback to PCM 48k/2ch/16bit",
                            e.code().0 as u32,
                            e.message(),
                        );
                        let fmt = pcm_format(48_000, 2, 16);
                        Ok((audio_client, FormatHolder::Owned(Box::new(fmt))))
                    }
                }
            }
        }
    }
}

fn pcm_format(sample_rate: u32, channels: u16, bits: u16) -> WAVEFORMATEX {
    let block_align = channels * bits / 8;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        wBitsPerSample: bits,
        nBlockAlign: block_align,
        nAvgBytesPerSec: sample_rate * block_align as u32,
        cbSize: 0,
    }
}

/// COM 回调:`ActivateAudioInterfaceAsync` 完成后把 event 置信号,让发起
/// 线程继续。本对象可能被 COM 在任意线程触发,所以只存 raw handle(usize)。
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    event: usize, // HANDLE.0 as usize;HANDLE 本身 !Send+!Sync
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { SetEvent(HANDLE(self.event as *mut _)) }
    }
}

unsafe fn activate_process_loopback_client(
    pid: u32,
    include_tree: bool,
) -> Result<IAudioClient> {
    diag!(
        "activate_process_loopback: pid={pid} include_tree={include_tree} \
         PARAMS size={}",
        size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(),
    );
    unsafe {
        let event: HANDLE = CreateEventW(None, false, false, PCWSTR::null())?;
        diag!("activate_process_loopback: event created {:p}", event.0);

        // params 的生命周期必须覆盖 Activate* 的异步执行;我们下面同步等待事件,
        // 等到 GetActivateResult 取完结果才 return,所以栈变量是安全的。
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: if include_tree {
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    } else {
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    },
                },
            },
        };

        // 手工组装 VT_BLOB PROPVARIANT,指向 AUDIOCLIENT_ACTIVATION_PARAMS。
        // 关键:包在 ManuallyDrop 里。PROPVARIANT 的 Drop 会调 PropVariantClear,
        // 对 VT_BLOB 会 CoTaskMemFree(pBlobData);但我们的 pBlobData 指向的是
        // **栈上的 `params`**,不是 COM 堆内存,走 CoTaskMemFree 会立刻 AV。
        // 既然 BLOB 数据本来就是栈上的,不需要任何清理,直接泄漏 PROPVARIANT
        // 外壳即可——反正 VT_BLOB 里除了 pBlobData 没有其他需要释放的资源。
        let mut pv: ManuallyDrop<PROPVARIANT> = ManuallyDrop::new(std::mem::zeroed());
        {
            let v00 = &mut *pv.Anonymous.Anonymous;
            v00.vt = VT_BLOB;
            v00.Anonymous.blob = BLOB {
                cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: &mut params as *mut _ as *mut u8,
            };
        }

        let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler {
            event: event.0 as usize,
        }
        .into();
        diag!("activate_process_loopback: handler built, calling ActivateAudioInterfaceAsync");

        let async_op_result = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        );
        let async_op: IActivateAudioInterfaceAsyncOperation = match async_op_result {
            Ok(op) => {
                diag!("activate_process_loopback: ActivateAudioInterfaceAsync returned OK (async pending)");
                op
            }
            Err(e) => {
                diag!(
                    "activate_process_loopback: ActivateAudioInterfaceAsync FAILED hr={:#010x} msg={}",
                    e.code().0 as u32,
                    e.message(),
                );
                let _ = CloseHandle(event);
                return Err(e.into());
            }
        };

        let wait = WaitForSingleObject(event, ACTIVATE_TIMEOUT_MS);
        diag!("activate_process_loopback: Wait returned {wait:?}");
        let _ = CloseHandle(event);
        if wait != WAIT_OBJECT_0 {
            return Err(WasapiError::UnsupportedFormat(format!(
                "激活 PID={pid} 的 per-process 音频客户端超时 (wait={wait:?})"
            )));
        }

        let mut hr = HRESULT(0);
        let mut iface: Option<IUnknown> = None;
        async_op.GetActivateResult(&mut hr, &mut iface)?;
        diag!(
            "activate_process_loopback: GetActivateResult hr={:#010x} has_iface={}",
            hr.0 as u32,
            iface.is_some(),
        );
        hr.ok()?;

        let iunk = iface.ok_or_else(|| {
            WasapiError::UnsupportedFormat("ActivateCompleted 未返回接口".into())
        })?;
        let client: IAudioClient = iunk.cast()?;
        diag!("activate_process_loopback: IAudioClient cast OK");
        // 显式按序丢弃,每一步打 checkpoint,定位究竟哪次 Release 炸。
        // 这些都是 COM 对象的 Release,顺序:iunk → async_op → handler。
        drop(iunk);
        diag!("activate_process_loopback: iunk dropped");
        drop(async_op);
        diag!("activate_process_loopback: async_op dropped");
        drop(handler);
        diag!("activate_process_loopback: handler dropped");
        Ok(client)
    }
}

unsafe fn inspect_format(pformat: *const WAVEFORMATEX) -> Result<FormatInfo> {
    // WAVEFORMATEX 在 windows-rs 里是 #[repr(packed)],不能直接借引用访问字段
    // (即便只读也是 UB);一律走 addr_of! + read_unaligned。
    use std::ptr::{addr_of, read_unaligned};
    unsafe {
        let tag = read_unaligned(addr_of!((*pformat).wFormatTag));
        let bits = read_unaligned(addr_of!((*pformat).wBitsPerSample));
        let channels = read_unaligned(addr_of!((*pformat).nChannels));
        let rate = read_unaligned(addr_of!((*pformat).nSamplesPerSec));
        let cb_size = read_unaligned(addr_of!((*pformat).cbSize));

        let kind = if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
            PcmKind::Float32
        } else if tag == WAVE_FORMAT_PCM as u16 && bits == 16 {
            PcmKind::Int16
        } else if tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
            let wex = pformat as *const WAVEFORMATEXTENSIBLE;
            let sub = read_unaligned(addr_of!((*wex).SubFormat));
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                PcmKind::Float32
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
                PcmKind::Int16
            } else {
                return Err(WasapiError::UnsupportedFormat(format!(
                    "EXTENSIBLE sub={sub:?} bits={bits}"
                )));
            }
        } else {
            return Err(WasapiError::UnsupportedFormat(format!(
                "tag={tag:#06x} bits={bits}"
            )));
        };

        Ok(FormatInfo {
            kind,
            channels,
            sample_rate: rate,
        })
    }
}

unsafe fn write_frames<W: std::io::Write + std::io::Seek>(
    writer: &mut WavWriter<W>,
    fmt: &FormatInfo,
    pdata: *const u8,
    num_frames: usize,
    silent: bool,
) -> Result<()> {
    let total = num_frames * fmt.channels as usize;
    match fmt.kind {
        PcmKind::Float32 => {
            if silent {
                for _ in 0..total {
                    writer.write_sample(0.0_f32)?;
                }
            } else {
                let src = unsafe { std::slice::from_raw_parts(pdata as *const f32, total) };
                for s in src {
                    writer.write_sample(*s)?;
                }
            }
        }
        PcmKind::Int16 => {
            if silent {
                for _ in 0..total {
                    writer.write_sample(0_i16)?;
                }
            } else {
                let src = unsafe { std::slice::from_raw_parts(pdata as *const i16, total) };
                for s in src {
                    writer.write_sample(*s)?;
                }
            }
        }
    }
    Ok(())
}
