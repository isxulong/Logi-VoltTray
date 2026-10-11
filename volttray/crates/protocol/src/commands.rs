//! 查询命令白名单（评审 P1-1：类型级封闭枚举）。
//!
//! 配对 / 写设置 / 固件类操作**在类型上不可构造**——本枚举只有查询变体，
//! 编译期即杜绝误发写命令。`encode_for` 额外按 features.json 的 kind 复核
//! （双保险，防止白名单表被改坏）。

use crate::error::HidppError;
use crate::features::registry;
use crate::frame::Frame;
use std::collections::BTreeMap;

/// PRD 协议附录中的 feature / 寄存器编号。
pub mod feature_ids {
    pub const IROOT: u16 = 0x0000;
    pub const IFEATURESET: u16 = 0x0001;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    /// HID++ 1.0 电量寄存器（PRD 协议附录）。
    pub const HIDPP1_BATTERY_REGISTER: u8 = 0x0D;
}

/// 单台设备解析出的 feature_id → feature_index 映射。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureTable {
    map: BTreeMap<u16, u8>,
}

impl FeatureTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, feature_id: u16, index: u8) {
        self.map.insert(feature_id, index);
    }

    pub fn get(&self, feature_id: u16) -> Option<u8> {
        self.map.get(&feature_id).copied()
    }

    /// IRoot 恒在 index 0（HID++ 2.0 规范）。
    pub fn iroot_index() -> u8 {
        0
    }
}

/// 查询命令封闭全集（P1-1）。新增任何写/配对语义都属于协议变更，需评审。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryCommand {
    /// IRoot fn1 ping：取协议版本。
    GetProtocolVersion,
    /// IFeatureSet fn0：按 feature_id 取 index。
    GetFeatureIndex { feature_id: u16 },
    /// UNIFIED_BATTERY(0x1004) fn0。
    GetBatteryUnified,
    /// BATTERY_STATUS(0x1000) fn0。
    GetBatteryStatus,
    /// BATTERY_VOLTAGE(0x1001) fn0。
    GetBatteryVoltage,
    /// HID++ 1.0 设备：读电量寄存器 0x0D。
    ReadHidpp1Register0x0D,
    /// HID Power Descriptor 回退：不在帧层编码，由传输层用 HID 特性报表读取。
    ReadHidPowerDescriptor,
}

impl QueryCommand {
    /// 编码为线上字节。`device_index` 为目标设备，`sw_id` 为请求配对 ID。
    ///
    /// 蓝图契约 `encode(&QueryCommand) -> Vec<u8>` 的落地形：feature_index
    /// 是每设备动态解析结果，必须随调用传入（见 docs/decisions.md D-02）。
    pub fn encode_for(&self, device_index: u8, sw_id: u8, features: &FeatureTable) -> Result<Vec<u8>, HidppError> {
        // P1-1 双保险：只许查询类 feature 上线。
        fn require_query(feature_id: u16) -> Result<(), HidppError> {
            if registry().is_query(feature_id) {
                Ok(())
            } else {
                Err(HidppError::NotEncodable("feature is not query-kind (whitelist violation)"))
            }
        }
        fn resolve(features: &FeatureTable, feature_id: u16) -> Result<u8, HidppError> {
            features
                .get(feature_id)
                .ok_or(HidppError::NotEncodable("feature index not resolved for device"))
        }

        let frame = match self {
            QueryCommand::GetProtocolVersion => {
                require_query(feature_ids::IROOT)?;
                Frame::new_short(device_index, FeatureTable::iroot_index(), 0x01, sw_id, [0, 0, 0])
            }
            QueryCommand::GetFeatureIndex { feature_id } => {
                // HID++ 2.0：GetFeatureIndex 是 IRoot（恒 index 0）的 fn0，
                // 不依赖任何已解析 index（否则无法自举）。
                require_query(feature_ids::IROOT)?;
                require_query(*feature_id)?;
                Frame::new_short(
                    device_index,
                    FeatureTable::iroot_index(),
                    0x00,
                    sw_id,
                    [(feature_id >> 8) as u8, (feature_id & 0xFF) as u8, 0],
                )
            }
            QueryCommand::GetBatteryUnified => {
                require_query(feature_ids::UNIFIED_BATTERY)?;
                let idx = resolve(features, feature_ids::UNIFIED_BATTERY)?;
                Frame::new_short(device_index, idx, 0x00, sw_id, [0, 0, 0])
            }
            QueryCommand::GetBatteryStatus => {
                require_query(feature_ids::BATTERY_STATUS)?;
                let idx = resolve(features, feature_ids::BATTERY_STATUS)?;
                Frame::new_short(device_index, idx, 0x00, sw_id, [0, 0, 0])
            }
            QueryCommand::GetBatteryVoltage => {
                require_query(feature_ids::BATTERY_VOLTAGE)?;
                let idx = resolve(features, feature_ids::BATTERY_VOLTAGE)?;
                Frame::new_short(device_index, idx, 0x00, sw_id, [0, 0, 0])
            }
            QueryCommand::ReadHidpp1Register0x0D => {
                // HID++ 1.0：byte2 即寄存器地址，无 feature 表可查。
                Frame::new_short(device_index, feature_ids::HIDPP1_BATTERY_REGISTER, 0x00, sw_id, [0, 0, 0])
            }
            QueryCommand::ReadHidPowerDescriptor => {
                return Err(HidppError::NotEncodable(
                    "HID power descriptor is read via HID feature report at transport layer",
                ));
            }
        };
        Ok(frame.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FeatureTable {
        let mut t = FeatureTable::new();
        t.insert(feature_ids::IFEATURESET, 1);
        t.insert(feature_ids::BATTERY_STATUS, 4);
        t.insert(feature_ids::BATTERY_VOLTAGE, 5);
        t.insert(feature_ids::UNIFIED_BATTERY, 6);
        t
    }

    #[test]
    fn battery_unified_encodes() {
        let bytes = QueryCommand::GetBatteryUnified.encode_for(0x01, 0x07, &table()).expect("encode");
        assert_eq!(bytes, vec![0x10, 0x01, 0x06, 0x07, 0, 0, 0]);
    }

    #[test]
    fn get_feature_index_encodes_via_iroot() {
        // IRoot 恒在 index 0，fn0；参数为 feature_id 大端。
        let bytes = QueryCommand::GetFeatureIndex { feature_id: 0x1004 }
            .encode_for(0x02, 0x03, &table())
            .expect("encode");
        assert_eq!(bytes, vec![0x10, 0x02, 0x00, 0x03, 0x10, 0x04, 0]);
    }

    #[test]
    fn unresolved_feature_is_not_encodable() {
        let t = FeatureTable::new();
        let err = QueryCommand::GetBatteryStatus.encode_for(0x01, 0x01, &t).expect_err("must fail");
        assert!(matches!(err, HidppError::NotEncodable(_)));
    }

    #[test]
    fn hid_power_descriptor_delegates_to_transport() {
        let err = QueryCommand::ReadHidPowerDescriptor
            .encode_for(0x01, 0x01, &table())
            .expect_err("must fail");
        assert!(matches!(err, HidppError::NotEncodable(_)));
    }
}
