//! hidapi 真实通道（feature=hid）。
//!
//! 设备筛选：VID=0x046D（Logitech）+ usage_page 0xFF43 / 0xFF00
//! （HID++ 厂商页；见 docs/decisions.md D-05）。

use crate::channel::{classify_io, HidChannel, IoError};
use std::ffi::CStr;
use std::time::Duration;
use volttray_protocol::LOGITECH_VID;

/// HID++ 厂商自定义 usage page。
pub const HIDPP_USAGE_PAGES: [u16; 2] = [0xFF43, 0xFF00];

pub struct HidDeviceChannel {
    dev: hidapi::HidDevice,
}

impl HidDeviceChannel {
    pub fn open_path(api: &hidapi::HidApi, path: &CStr) -> Result<Self, IoError> {
        api.open_path(path)
            .map(|dev| HidDeviceChannel { dev })
            .map_err(|e| classify_io(&e.to_string()))
    }
}

impl HidChannel for HidDeviceChannel {
    fn write(&mut self, data: &[u8]) -> Result<usize, IoError> {
        self.dev
            .write(data)
            .map_err(|e| classify_io(&e.to_string()))
    }

    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, IoError> {
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        match self.dev.read_timeout(buf, ms) {
            Ok(0) => Err(IoError::Timeout),
            Ok(n) => Ok(n),
            Err(e) => Err(classify_io(&e.to_string())),
        }
    }

    fn get_feature_report(&mut self, report_id: u8, buf: &mut [u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Err(IoError::Other("empty feature report buffer".into()));
        }
        buf[0] = report_id;
        self.dev
            .get_feature_report(buf)
            .map_err(|e| classify_io(&e.to_string()))
    }
}

/// 枚举所有 Logitech HID++ 候选接口。
pub fn enumerate_logitech(api: &hidapi::HidApi) -> Vec<hidapi::DeviceInfo> {
    api.device_list()
        .filter(|d| d.vendor_id() == LOGITECH_VID && HIDPP_USAGE_PAGES.contains(&d.usage_page()))
        .cloned()
        .collect()
}
