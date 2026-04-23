//! 枚举 WASAPI 输入/输出端点设备。

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::core::GUID;

use super::{Result, run_on_mta};

/// `PKEY_Device_FriendlyName`,写死常量避免多开一个 feature。
/// 来源: `<functiondiscoverykeys_devpkey.h>`
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

#[derive(Debug, Clone)]
pub struct EndpointDevice {
    pub id: String,           // IMMDevice::GetId 返回的设备 ID(WASAPI 内部标识)
    pub friendly_name: String, // 用户可读名称,例如 "扬声器 (Realtek...)"
    pub is_default: bool,
    pub flow: EndpointFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointFlow {
    /// 输出端点(扬声器/耳机)—— 用于 **系统 loopback** 捕获
    Render,
    /// 输入端点(麦克风/线路输入)—— 用于 **麦克风** 捕获
    Capture,
}

impl From<EDataFlow> for EndpointFlow {
    fn from(f: EDataFlow) -> Self {
        if f == eCapture { Self::Capture } else { Self::Render }
    }
}

pub fn list_input_devices() -> Result<Vec<EndpointDevice>> {
    run_on_mta(|| enumerate(eCapture))
}

pub fn list_output_devices() -> Result<Vec<EndpointDevice>> {
    run_on_mta(|| enumerate(eRender))
}

fn enumerate(flow: EDataFlow) -> Result<Vec<EndpointDevice>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

        let default_id = enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .ok()
            .and_then(|d| device_id(&d).ok());

        let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dev = collection.Item(i)?;
            let id = device_id(&dev)?;
            let friendly_name = device_friendly_name(&dev).unwrap_or_else(|_| "<未知>".into());
            let is_default = default_id.as_deref() == Some(&id);
            out.push(EndpointDevice {
                id,
                friendly_name,
                is_default,
                flow: flow.into(),
            });
        }
        Ok(out)
    }
}

unsafe fn device_id(dev: &IMMDevice) -> Result<String> {
    unsafe {
        let pwstr = dev.GetId()?;
        let s = pwstr.to_string().map_err(|_| super::WasapiError::BadString)?;
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(s)
    }
}

unsafe fn device_friendly_name(dev: &IMMDevice) -> Result<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ)?;
        let prop = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME)?;
        // PROPVARIANT 在 windows crate 0.62+ 是 RAII 包装,Drop 时自动 PropVariantClear。
        let s = prop.to_string();
        Ok(s)
    }
}

