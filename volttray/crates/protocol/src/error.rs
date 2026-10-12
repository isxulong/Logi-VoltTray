//! 协议层错误类型。
//!
//! PRD 要求：错误枚举穷尽、无通配匹配臂；规范外取值以显式变体保留原始数据。

use core::fmt;

/// 错误帧所属协议族。同一错误码在两族中的语义不同（红队差异 F5）：
/// - Hidpp1：0x08 = UNKNOWN_DEVICE（设备休眠/离线）
/// - Hidpp2：0x08 = Busy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorFamily {
    /// 0x8F 错误帧（HID++ 1.0）。
    Hidpp1,
    /// 0xFF 错误帧（HID++ 2.0）。
    Hidpp2,
}

impl ErrorFamily {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hidpp1 => "hidpp1",
            Self::Hidpp2 => "hidpp2",
        }
    }
}

/// HID++ 设备错误帧中的错误码（基线语义按 HID++ 2.0；1.0 语义差异见 [`ErrorFamily`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceErrorCode {
    /// 0x01
    Unknown,
    /// 0x02
    InvalidArgument,
    /// 0x03
    OutOfSequence,
    /// 0x04
    InvalidParamValue,
    /// 0x05
    WrongDevice,
    /// 0x06
    InvalidFeatureIndex,
    /// 0x07
    InvalidFunctionId,
    /// 0x08
    Busy,
    /// 0x09
    Unsupported,
    /// 规范之外的错误码，保留原始值（禁止静默归并）。
    UnknownCode(u8),
}

impl DeviceErrorCode {
    pub fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::Unknown,
            0x02 => Self::InvalidArgument,
            0x03 => Self::OutOfSequence,
            0x04 => Self::InvalidParamValue,
            0x05 => Self::WrongDevice,
            0x06 => Self::InvalidFeatureIndex,
            0x07 => Self::InvalidFunctionId,
            0x08 => Self::Busy,
            0x09 => Self::Unsupported,
            other => Self::UnknownCode(other),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Unknown => 0x01,
            Self::InvalidArgument => 0x02,
            Self::OutOfSequence => 0x03,
            Self::InvalidParamValue => 0x04,
            Self::WrongDevice => 0x05,
            Self::InvalidFeatureIndex => 0x06,
            Self::InvalidFunctionId => 0x07,
            Self::Busy => 0x08,
            Self::Unsupported => 0x09,
            Self::UnknownCode(raw) => raw,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "ERR_UNKNOWN",
            Self::InvalidArgument => "ERR_INVALID_ARGUMENT",
            Self::OutOfSequence => "ERR_OUT_OF_SEQUENCE",
            Self::InvalidParamValue => "ERR_INVALID_PARAM_VALUE",
            Self::WrongDevice => "ERR_WRONG_DEVICE",
            Self::InvalidFeatureIndex => "ERR_INVALID_FEATURE_INDEX",
            Self::InvalidFunctionId => "ERR_INVALID_FUNCTION_ID",
            Self::Busy => "ERR_BUSY",
            Self::Unsupported => "ERR_UNSUPPORTED",
            Self::UnknownCode(_) => "ERR_UNKNOWN_CODE",
        }
    }
}

/// 帧畸形原因（解析侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MalformedKind {
    /// 空输入。
    EmptyFrame,
    /// 首字节不是 0x10/0x11。
    BadReportId(u8),
    /// 长度与 report id 不匹配。
    BadLength {
        report: u8,
        expected: usize,
        actual: usize,
    },
}

/// 协议层统一错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HidppError {
    /// 设备返回的错误帧（feature 0xFF 为 2.0，0x8F 为 1.0）。
    /// `feature_index` 为被拒 feature 的回显（2.0）或寄存器地址回显（1.0）。
    Device {
        device_index: u8,
        family: ErrorFamily,
        feature_index: u8,
        code: DeviceErrorCode,
    },
    /// 帧格式非法。
    Malformed(MalformedKind),
    /// 该命令无法在 HID++ 帧层编码（由传输层以其它方式处理，如 HID 特性报表）。
    NotEncodable(&'static str),
}

impl fmt::Display for HidppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device { device_index, family, feature_index, code } => write!(
                f,
                "device error: dev=0x{device_index:02X} family={} feat=0x{feature_index:02X} code={} (0x{:02X})",
                family.name(),
                code.name(),
                code.code()
            ),
            Self::Malformed(kind) => match kind {
                MalformedKind::EmptyFrame => write!(f, "malformed frame: empty input"),
                MalformedKind::BadReportId(id) => {
                    write!(f, "malformed frame: bad report id 0x{id:02X}")
                }
                MalformedKind::BadLength { report, expected, actual } => write!(
                    f,
                    "malformed frame: report 0x{report:02X} expects {expected} bytes, got {actual}"
                ),
            },
            Self::NotEncodable(what) => write!(f, "command not encodable at frame layer: {what}"),
        }
    }
}

impl std::error::Error for HidppError {}
