//! HID++ 帧结构与编解码。
//!
//! 帧布局（PRD 协议附录）：
//! - 短帧 7 字节：`[0x10, dev, feat, func<<4|sw_id, p0, p1, p2]`
//! - 长帧 20 字节：`[0x11, dev, feat, func<<4|sw_id, p0 .. p15]`
//! - 错误帧：feat = 0xFF（2.0）或 0x8F（1.0），`p0` = 原 feature，`p1` = 错误码。

use crate::error::{DeviceErrorCode, HidppError, MalformedKind};

pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;
pub const ERROR_FEATURE_2_0: u8 = 0xFF;
pub const ERROR_FEATURE_1_0: u8 = 0x8F;
pub const MAX_PARAMS: usize = 16;

/// 一帧 HID++ 消息（请求或正常响应；设备错误帧由 `decode` 直接转成 `Err`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub report: u8,
    pub device_index: u8,
    pub feature_index: u8,
    /// 4 bit 功能号。
    pub function: u8,
    /// 4 bit 软件 ID（请求/响应配对用）。
    pub sw_id: u8,
    pub params: [u8; MAX_PARAMS],
    /// 有效参数字节数：短帧 3，长帧 16。
    pub param_len: usize,
}

impl Frame {
    fn new(report: u8, device_index: u8, feature_index: u8, function: u8, sw_id: u8, params: &[u8]) -> Self {
        let mut buf = [0u8; MAX_PARAMS];
        buf[..params.len()].copy_from_slice(params);
        Frame {
            report,
            device_index,
            feature_index,
            function: function & 0x0F,
            sw_id: sw_id & 0x0F,
            params: buf,
            param_len: params.len(),
        }
    }

    pub fn new_short(device_index: u8, feature_index: u8, function: u8, sw_id: u8, params: [u8; 3]) -> Self {
        Frame::new(REPORT_SHORT, device_index, feature_index, function, sw_id, &params)
    }

    pub fn new_long(device_index: u8, feature_index: u8, function: u8, sw_id: u8, params: [u8; 16]) -> Self {
        Frame::new(REPORT_LONG, device_index, feature_index, function, sw_id, &params)
    }

    pub fn is_long(&self) -> bool {
        self.report == REPORT_LONG
    }

    /// 序列化为线上字节（含首字节 report id，可直接交给 hidapi write）。
    pub fn encode(&self) -> Vec<u8> {
        let total = if self.is_long() { LONG_LEN } else { SHORT_LEN };
        let mut out = Vec::with_capacity(total);
        out.push(self.report);
        out.push(self.device_index);
        out.push(self.feature_index);
        out.push((self.function << 4) | (self.sw_id & 0x0F));
        out.extend_from_slice(&self.params[..self.param_len.min(MAX_PARAMS)]);
        out
    }

    /// 有效参数切片。
    pub fn params(&self) -> &[u8] {
        &self.params[..self.param_len.min(MAX_PARAMS)]
    }

    /// 解析线上字节。设备错误帧返回 `Err(HidppError::Device{..})`。
    pub fn decode(bytes: &[u8]) -> Result<Frame, HidppError> {
        let (&report, rest) = bytes.split_first().ok_or(HidppError::Malformed(MalformedKind::EmptyFrame))?;
        let expected = match report {
            REPORT_SHORT => SHORT_LEN,
            REPORT_LONG => LONG_LEN,
            other => return Err(HidppError::Malformed(MalformedKind::BadReportId(other))),
        };
        if bytes.len() != expected {
            return Err(HidppError::Malformed(MalformedKind::BadLength {
                report,
                expected,
                actual: bytes.len(),
            }));
        }
        let device_index = rest[0];
        let feature_index = rest[1];
        let fsw = rest[2];
        if feature_index == ERROR_FEATURE_2_0 || feature_index == ERROR_FEATURE_1_0 {
            return Err(HidppError::Device {
                device_index,
                feature_index: rest[3],
                code: DeviceErrorCode::from_code(rest[4]),
            });
        }
        Ok(Frame::new(
            report,
            device_index,
            feature_index,
            fsw >> 4,
            fsw & 0x0F,
            &rest[3..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_frame_roundtrip() {
        let f = Frame::new_short(0x01, 0x05, 0x0F, 0x0F, [0x50, 0x00, 0x01]);
        let bytes = f.encode();
        assert_eq!(bytes.len(), SHORT_LEN);
        assert_eq!(bytes[0], REPORT_SHORT);
        assert_eq!(bytes[3], 0xFF);
        let back = Frame::decode(&bytes).expect("decode");
        assert_eq!(back, f);
    }

    #[test]
    fn long_frame_roundtrip() {
        let f = Frame::new_long(0xFF, 0x01, 0x1F, 0x00, [0xAA; 16]);
        let bytes = f.encode();
        assert_eq!(bytes.len(), LONG_LEN);
        let back = Frame::decode(&bytes).expect("decode");
        assert_eq!(back.function, 0x0F); // 4 bit 截断
        assert_eq!(back, Frame::new_long(0xFF, 0x01, 0x0F, 0x00, [0xAA; 16]));
    }

    #[test]
    fn error_frame_2_0_decodes_to_device_error() {
        // feat=0xFF, p0=原 feature 0x05, p1=错误码 0x06
        let bytes = [0x11, 0x01, 0xFF, 0x00, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let err = Frame::decode(&bytes).expect_err("must be device error");
        match err {
            HidppError::Device { device_index, feature_index, code } => {
                assert_eq!(device_index, 0x01);
                assert_eq!(feature_index, 0x05);
                assert_eq!(code, DeviceErrorCode::InvalidFeatureIndex);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn error_frame_1_0_decodes_to_device_error() {
        let bytes = [0x10, 0x02, 0x8F, 0x00, 0x0D, 0x02, 0x00];
        let err = Frame::decode(&bytes).expect_err("must be device error");
        match err {
            HidppError::Device { code, feature_index, .. } => {
                assert_eq!(code, DeviceErrorCode::InvalidArgument);
                assert_eq!(feature_index, 0x0D);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
