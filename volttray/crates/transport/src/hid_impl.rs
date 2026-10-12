//! hidapi 真实通道（feature=hid）。
//!
//! v0.2.0 接口配对修正（红队差异 #4 / 参考实现 `open_channels` 实证）：
//! HID++ 短报文口（usage=1）与长报文口（usage=2）是同一物理设备的两个
//! HID collection，必须按「去掉 `&ColNN` 后缀的路径」配对成同一个通道，
//! 各自打开句柄：长帧写长口、短帧写短口。
//!
//! 设备筛选：VID=0x046D（Logitech）+ usage_page 0xFF00/0xFF43（HID++ 厂商页）。
//! 真实通道额外覆盖 [`HidChannel::drain`]（请求前排空滞留旧帧）。

use crate::channel::{classify_io, HidChannel, IoError};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::time::Duration;
use volttray_protocol::frame::{REPORT_LONG, REPORT_SHORT, SHORT_LEN};
use volttray_protocol::LOGITECH_VID;

/// HID++ 厂商自定义 usage page。
pub const HIDPP_USAGE_PAGES: [u16; 2] = [0xFF00, 0xFF43];
/// 短报文口 usage（HID++ collection）。
pub const USAGE_SHORT: u16 = 1;
/// 长报文口 usage。
pub const USAGE_LONG: u16 = 2;
/// drain 单次最多排空的滞留帧数（参考实现实证：80）。
pub const DRAIN_MAX_FRAMES: usize = 80;

/// 一个配对完成的 HID++ 通道：短/长两个 collection 句柄。
pub struct HidDeviceChannel {
    short: Option<hidapi::HidDevice>,
    long: Option<hidapi::HidDevice>,
}

impl HidDeviceChannel {
    /// 打开配对通道。`short_path` 缺失时短帧也走长口（参考实现同款兜底）。
    pub fn open(
        api: &hidapi::HidApi,
        short_path: Option<&CStr>,
        long_path: Option<&CStr>,
    ) -> Result<Self, IoError> {
        let open = |p: Option<&CStr>| -> Result<Option<hidapi::HidDevice>, IoError> {
            match p {
                Some(p) => api
                    .open_path(p)
                    .map(Some)
                    .map_err(|e| classify_io(&e.to_string())),
                None => Ok(None),
            }
        };
        let open_short = || -> Result<Option<hidapi::HidDevice>, IoError> {
            // 参考实现同款兜底：缺短口路径时用长口路径再开一个句柄顶替短帧读写。
            open(short_path.or(long_path))
        };
        let short = open_short()?;
        let long = open(long_path)?;
        if short.is_none() && long.is_none() {
            return Err(IoError::Other("no hidpp collection could be opened".into()));
        }
        Ok(HidDeviceChannel { short, long })
    }

    fn read_handle(
        dev: &Option<hidapi::HidDevice>,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, IoError> {
        let Some(d) = dev else {
            return Err(IoError::Other("collection not present".into()));
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        match d.read_timeout(buf, ms) {
            Ok(0) => Err(IoError::Timeout),
            Ok(n) => Ok(n),
            Err(e) => Err(classify_io(&e.to_string())),
        }
    }
}

impl HidChannel for HidDeviceChannel {
    fn write(&mut self, data: &[u8]) -> Result<usize, IoError> {
        // 长帧优先写长口；无长口时按参考实现降级：改写为短帧（截断参数）。
        if data.first() == Some(&REPORT_LONG) && self.long.is_none() {
            let mut short = [0u8; SHORT_LEN];
            let keep = data.len().min(SHORT_LEN);
            short[..keep].copy_from_slice(&data[..keep]);
            short[0] = REPORT_SHORT;
            let Some(d) = self.short.as_mut() else {
                return Err(IoError::Other("no writable collection".into()));
            };
            return d.write(&short).map_err(|e| classify_io(&e.to_string()));
        }
        let dev = if data.first() == Some(&REPORT_LONG) {
            self.long.as_mut()
        } else {
            self.short.as_mut()
        };
        let Some(d) = dev else {
            // 无长口却要写长帧：走上面的降级分支；无短口属异常。
            return Err(IoError::Other("no writable collection".into()));
        };
        d.write(data).map_err(|e| classify_io(&e.to_string()))
    }

    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, IoError> {
        // 先长口后短口（参考实现同序），任一口出帧即返回。
        if self.long.is_some() {
            match Self::read_handle(&self.long, buf, timeout) {
                Ok(n) => return Ok(n),
                Err(IoError::Timeout) => {}
                Err(e) => return Err(e),
            }
        }
        Self::read_handle(&self.short, buf, timeout)
    }

    fn drain(&mut self) {
        let mut buf = [0u8; 64];
        for dev in [&mut self.long, &mut self.short] {
            let Some(d) = dev else {
                continue;
            };
            for _ in 0..DRAIN_MAX_FRAMES {
                match d.read_timeout(&mut buf, 0) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        }
    }

    fn get_feature_report(&mut self, report_id: u8, buf: &mut [u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Err(IoError::Other("empty feature report buffer".into()));
        }
        buf[0] = report_id;
        let dev = self.short.as_ref().or(self.long.as_ref());
        let Some(d) = dev else {
            return Err(IoError::Other("no collection".into()));
        };
        d.get_feature_report(buf)
            .map_err(|e| classify_io(&e.to_string()))
    }
}

/// 一个待打开的配对通道（枚举产物）。
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub pid: u16,
    /// 去掉 collection 后缀的路径标识（稳定 key）。
    pub tag: String,
    /// 接收器产品名（首个接口的 product_string，可能为空）。
    pub product: String,
    pub short_path: Option<std::ffi::CString>,
    pub long_path: Option<std::ffi::CString>,
}

impl ChannelInfo {
    /// 通道级稳定 ID（托盘设备的 id 前缀）。
    pub fn key(&self) -> String {
        format!("{:04X}|{}", self.pid, self.tag)
    }
}

/// 去掉 HID 路径的 `&ColNN` collection 后缀，得到短/长接口共用的配对 key
/// （参考实现 `_path_tag` 实证）。
pub fn path_tag(path: &CStr) -> String {
    let s = path.to_string_lossy().into_owned();
    let seg: Vec<&str> = s.split('#').collect();
    let base = if seg.len() > 1 { seg[1] } else { s.as_str() };
    // Windows HID 路径的 collection 后缀形如 `&Col02`。
    if let Some(pos) = base.find("&Col") {
        base[..pos].to_string()
    } else {
        base.to_string()
    }
}

/// 枚举所有 Logitech HID++ 候选接口，并按 (pid, tag) 把短/长 collection 配对成通道。
pub fn enumerate_channels(api: &hidapi::HidApi) -> Vec<ChannelInfo> {
    let mut grouped: BTreeMap<(u16, String), ChannelInfo> = BTreeMap::new();
    for d in api.device_list() {
        if d.vendor_id() != LOGITECH_VID {
            continue;
        }
        if !HIDPP_USAGE_PAGES.contains(&d.usage_page()) {
            continue;
        }
        let usage = d.usage();
        if usage != USAGE_SHORT && usage != USAGE_LONG {
            continue; // 非 HID++ 报文口（充电状态 LED 等）不碰。
        }
        let tag = path_tag(d.path());
        let key = (d.product_id(), tag.clone());
        let entry = grouped.entry(key).or_insert_with(|| ChannelInfo {
            pid: d.product_id(),
            tag: String::new(),
            product: String::new(),
            short_path: None,
            long_path: None,
        });
        if entry.tag.is_empty() {
            entry.tag = tag;
        }
        if entry.product.is_empty() {
            entry.product = d.product_string().unwrap_or("").to_string();
        }
        match usage {
            USAGE_SHORT if entry.short_path.is_none() => {
                entry.short_path = Some(d.path().to_owned())
            }
            USAGE_LONG if entry.long_path.is_none() => entry.long_path = Some(d.path().to_owned()),
            _ => {}
        }
    }
    grouped
        .into_values()
        .filter(|c| c.short_path.is_some() || c.long_path.is_some())
        .collect()
}

/// 兼容旧调用方：原始（未配对）接口列表。
pub fn enumerate_logitech(api: &hidapi::HidApi) -> Vec<hidapi::DeviceInfo> {
    api.device_list()
        .filter(|d| d.vendor_id() == LOGITECH_VID && HIDPP_USAGE_PAGES.contains(&d.usage_page()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_tag_strips_collection_suffix() {
        let p = std::ffi::CString::new(
            "\\\\?\\hid#vid_046d&pid_c52b#6&2a0f0d1e&0&0000#{c889b53d-...}&Col02",
        )
        .unwrap();
        let tag = path_tag(&p);
        assert!(!tag.contains("&Col02"), "got {tag}");
        let again = path_tag(&std::ffi::CString::new(tag.clone()).unwrap());
        assert_eq!(again, tag, "must be idempotent");
    }
}
