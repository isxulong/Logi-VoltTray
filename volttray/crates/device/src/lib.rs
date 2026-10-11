//! volttray-device —— 设备抽象层。
//!
//! 职责（蓝图契约）：设备模型与生命周期状态机（5.6）、设备名消毒（P1-5）、
//! 电量策略链（6.3）、接收器 PID 分类（PRD 协议附录）。

pub mod battery;
pub mod handshake;
pub mod sanitize;
pub mod state;

use volttray_protocol::{PID_BOLT, PID_LIGHTSPEED, PID_UNIFYING};

/// 连接链路类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    Unifying,
    Bolt,
    Lightspeed,
    /// 有线 / 蓝牙直连。
    Direct,
    Unknown,
}

impl LinkKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unifying => "unifying",
            Self::Bolt => "bolt",
            Self::Lightspeed => "lightspeed",
            Self::Direct => "direct",
            Self::Unknown => "unknown",
        }
    }
}

/// 按接收器/设备 PID 分类（PRD 协议附录）。
pub fn classify_pid(pid: u16) -> LinkKind {
    if PID_UNIFYING.contains(&pid) {
        LinkKind::Unifying
    } else if PID_BOLT.contains(&pid) {
        LinkKind::Bolt
    } else if PID_LIGHTSPEED.contains(&pid) {
        LinkKind::Lightspeed
    } else {
        LinkKind::Unknown
    }
}

/// 一台受管设备。
#[derive(Debug, Clone)]
pub struct Device {
    /// 稳定 ID（HID 路径散列或 device_index 派生，由 app 分配）。
    pub id: String,
    /// 已消毒的显示名（P1-5）。
    pub name: String,
    pub link: LinkKind,
    /// HID++ 设备索引（1..=255；0xFF 为接收器直连设备）。
    pub device_index: u8,
    pub state: state::DeviceState,
    pub battery: Option<battery::BatteryReading>,
    /// HID++ 1.0 设备（无 IFeatureSet）。
    pub is_hidpp1: bool,
}

impl Device {
    pub fn new(id: String, raw_name: &str, link: LinkKind, device_index: u8, is_hidpp1: bool) -> Self {
        Device {
            id,
            name: sanitize::device_name(raw_name),
            link,
            device_index,
            state: state::DeviceState::Connected,
            battery: None,
            is_hidpp1,
        }
    }

    /// 状态迁移（非法迁移 panic，见 state 模块）。
    pub fn set_state(&mut self, to: state::DeviceState) {
        self.state = state::transition(self.state, to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_classification() {
        assert_eq!(classify_pid(0xC52B), LinkKind::Unifying);
        assert_eq!(classify_pid(0xC532), LinkKind::Unifying);
        assert_eq!(classify_pid(0xC548), LinkKind::Bolt);
        assert_eq!(classify_pid(0xC539), LinkKind::Lightspeed);
        assert_eq!(classify_pid(0xC077), LinkKind::Unknown);
    }

    #[test]
    fn device_name_is_sanitized_on_construction() {
        let d = Device::new("d1".into(), "MX\u{0007}Master", LinkKind::Bolt, 1, false);
        assert_eq!(d.name, "MXMaster");
    }
}
