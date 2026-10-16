//! volttray-device —— 设备抽象层。
//!
//! 职责（蓝图契约）：设备模型与生命周期状态机（5.6）、设备名消毒（P1-5）、
//! 电量策略链（6.3）、接收器 PID 分类（PRD 协议附录）。

pub mod battery;
pub mod handshake;
pub mod identity;
pub mod name;
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

/// 物理设备身份（T6 有线模式重复条目去重）。
///
/// 端点身份（`Device::id` = `{pid}|{tag}#{slot}`）在同一物理鼠标插线后会
/// 枚举出多个（有线 7 个 USB 接口 × 0xFF 槽）；物理身份跨端点不变，用于
/// 把多条端点注册收敛为一个条目。
///
/// 合并铁律（保守规则）：只有双方都持有已证实的 [`PhysicalKey::UnitId`]
/// 且值相等才可合并；[`PhysicalKey::Name`] 只是占位/观测标记，永不参与
/// 合并——身份不明宁可保留分开条目，绝不误并两台独立物理鼠标。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PhysicalKey {
    /// HID++ unitId（DEVICE_FW_VERSION fn0 应答 params[1:5]，已证实）。
    UnitId(String),
    /// 清洗后的显示名（unitId 取不到时的降级占位；不参与合并）。
    Name(String),
}

impl PhysicalKey {
    /// 是否为已证实的物理身份（仅 unitId）。
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::UnitId(_))
    }

    /// 保守合并判据：双方都 proven 且相等才 true。
    pub fn proven_matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnitId(a), Self::UnitId(b)) => a == b,
            _ => false,
        }
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
    /// 物理设备身份（T6）。构造时降级为 Name（已消毒名），注册期读到
    /// unitId 后经 [`Device::set_unit_id`] 升级为 proven 身份。
    pub physical_key: PhysicalKey,
}

impl Device {
    pub fn new(
        id: String,
        raw_name: &str,
        link: LinkKind,
        device_index: u8,
        is_hidpp1: bool,
    ) -> Self {
        Device {
            id,
            name: sanitize::device_name(raw_name),
            link,
            device_index,
            state: state::DeviceState::Connected,
            battery: None,
            is_hidpp1,
            physical_key: PhysicalKey::Name(sanitize::device_name(raw_name)),
        }
    }

    /// 用注册期读到的 unitId 升级物理身份（T6；空串忽略）。
    pub fn set_unit_id(&mut self, unit_id: &str) {
        if !unit_id.is_empty() {
            self.physical_key = PhysicalKey::UnitId(unit_id.to_string());
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

    #[test]
    fn physical_key_defaults_to_sanitized_name_and_upgrades() {
        let mut d = Device::new("d1".into(), "MX\u{0007}Master", LinkKind::Bolt, 1, false);
        // 构造即降级身份：已消毒名，非 proven。
        assert_eq!(d.physical_key, PhysicalKey::Name("MXMaster".into()));
        assert!(!d.physical_key.is_proven());
        d.set_unit_id("39414243");
        assert_eq!(d.physical_key, PhysicalKey::UnitId("39414243".into()));
        assert!(d.physical_key.is_proven());
        // 空串不降级已有身份。
        d.set_unit_id("");
        assert!(d.physical_key.is_proven());
    }

    #[test]
    fn proven_matches_only_when_both_unit_ids_equal() {
        let a = PhysicalKey::UnitId("AAAA0001".into());
        let b = PhysicalKey::UnitId("AAAA0001".into());
        let c = PhysicalKey::UnitId("BBBB0002".into());
        let n1 = PhysicalKey::Name("PRO X 2".into());
        let n2 = PhysicalKey::Name("PRO X 2".into());
        assert!(a.proven_matches(&b));
        assert!(!a.proven_matches(&c));
        // 同名 / 一名一 unitId 都不算证实一致（保守规则）。
        assert!(!n1.proven_matches(&n2));
        assert!(!a.proven_matches(&n1));
        assert!(!n1.proven_matches(&a));
    }
}
