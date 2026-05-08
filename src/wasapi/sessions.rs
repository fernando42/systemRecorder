//! 枚举当前正在(或曾经)使用默认输出端点的音频会话。
//!
//! 用来给 UI 列出"正在发声的 App"清单,供用户挑选 PID 做 per-process loopback。

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
    IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
    eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::core::Interface;

use super::{Result, WasapiError, run_on_mta};

#[derive(Debug, Clone)]
pub struct AudioSession {
    pub pid: u32,
    pub exe_path: String,     // 完整 exe 路径(拿不到时为空字符串)
    pub display_name: String, // SetDisplayName 提供的友好名;多数 App 不会设,会是空
    pub state: SessionState,
    pub is_system_sounds: bool, // Windows 的系统提示音会话
}

impl AudioSession {
    /// UI 里给用户看的名字。
    ///
    /// 优先级:系统提示音标记 → exe 文件名 → display_name(若不是 MUI 资源引用)→ PID。
    /// 之所以把 display_name 排到 exe 后面:多数 App 根本不调 `SetDisplayName`,
    /// 系统自动填的值要么是空,要么是 `@xxx.dll,-123` 形式的资源 ID,用户看不懂。
    pub fn best_label(&self) -> String {
        if self.is_system_sounds {
            return "Windows 系统提示音".to_string();
        }
        if !self.exe_path.is_empty()
            && let Some(name) = std::path::Path::new(&self.exe_path).file_name()
        {
            return name.to_string_lossy().into_owned();
        }
        if !self.display_name.is_empty() && !self.display_name.starts_with('@') {
            return self.display_name.clone();
        }
        format!("PID {}", self.pid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,   // 正在出声
    Inactive, // 已打开音频客户端但当前无数据
    Expired,  // 客户端已关闭
}

pub fn list_audio_sessions() -> Result<Vec<AudioSession>> {
    run_on_mta(enumerate_sessions)
}

fn enumerate_sessions() -> Result<Vec<AudioSession>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;

        let session_manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let enumerator = session_manager.GetSessionEnumerator()?;
        let count = enumerator.GetCount()?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ctrl = enumerator.GetSession(i)?;
            let ctrl2: IAudioSessionControl2 = ctrl.cast()?;

            let pid = ctrl2.GetProcessId().unwrap_or(0);
            // HRESULT: S_OK = 是系统提示音,S_FALSE = 不是。
            // 两者都是 "非错误",不能用 is_ok()。
            let is_system_sounds = ctrl2.IsSystemSoundsSession() == S_OK;

            let display_name = ctrl
                .GetDisplayName()
                .ok()
                .and_then(|p| {
                    let s = p.to_string().ok();
                    windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
                    s
                })
                .unwrap_or_default();

            let state = match ctrl.GetState()? {
                s if s == AudioSessionStateActive => SessionState::Active,
                s if s == AudioSessionStateInactive => SessionState::Inactive,
                s if s == AudioSessionStateExpired => SessionState::Expired,
                _ => SessionState::Inactive,
            };

            let exe_path = if pid != 0 && !is_system_sounds {
                pid_to_exe_path(pid).unwrap_or_default()
            } else {
                String::new()
            };

            out.push(AudioSession {
                pid,
                exe_path,
                display_name,
                state,
                is_system_sounds,
            });
        }

        // 可用性排序:Active → Inactive → Expired,再按 label
        out.sort_by(|a, b| {
            let sa = a.state as u8;
            let sb = b.state as u8;
            sa.cmp(&sb)
                .then_with(|| a.best_label().cmp(&b.best_label()))
        });

        Ok(out)
    }
}

fn pid_to_exe_path(pid: u32) -> Result<String> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )?;
        let mut buf = [0u16; 260];
        let len = GetModuleFileNameExW(Some(handle), None, &mut buf);
        windows::Win32::Foundation::CloseHandle(handle).ok();
        if len == 0 {
            return Err(WasapiError::BadString);
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }
}
