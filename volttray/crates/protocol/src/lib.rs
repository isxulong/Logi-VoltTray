//! volttray-protocol —— HID++ 协议组帧/解帧。
//!
//! 纯计算 crate：不做任何 I/O、不依赖系统时间，便于黄金样例与 proptest 验证。
//! 依据：《Logi VoltTray 产品文档 v1.0》协议附录 + 实现蓝图 crate 契约。

pub mod commands;
pub mod error;
pub mod features;
pub mod frame;
pub mod voltage;

pub use commands::{feature_ids, FeatureTable, QueryCommand};
pub use error::{DeviceErrorCode, HidppError, MalformedKind};
pub use features::{registry, FeatureInfo, FeatureKind, FeatureRegistry};
pub use frame::{Frame, ERROR_FEATURE_1_0, ERROR_FEATURE_2_0, LONG_LEN, REPORT_LONG, REPORT_SHORT, SHORT_LEN};

/// Logitech USB Vendor ID（PRD 协议附录）。
pub const LOGITECH_VID: u16 = 0x046D;
/// Unifying 接收器 PID（PRD 协议附录）。
pub const PID_UNIFYING: [u16; 2] = [0xC52B, 0xC532];
/// Bolt 接收器 PID。
pub const PID_BOLT: [u16; 1] = [0xC548];
/// Lightspeed 接收器 PID。
pub const PID_LIGHTSPEED: [u16; 1] = [0xC539];
