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
    /// HID++ 1.0 电量寄存器：0x0D（真实百分比）与 0x07（粗略档位）。
    pub const HIDPP1_REG_BATTERY_CHARGE: u8 = 0x0D;
    pub const HIDPP1_REG_BATTERY_STATUS: u8 = 0x07;
    /// HID++ 1.0 读子地址（参考实现实证：req = 0x8100 | reg → byte2=0x81，byte3=reg）。
    pub const HIDPP1_READ_SUB: u8 = 0x81;
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
    /// UNIFIED_BATTERY(0x1004) fn1 getStatus：[电量, 档位, 充电状态, 外接电源]。
    GetBatteryUnified,
    /// BATTERY_STATUS(0x1000) fn0：[电量, 下一档, 充电状态]。
    GetBatteryStatus,
    /// BATTERY_VOLTAGE(0x1001) fn0：[电压高, 电压低, 标志]。
    GetBatteryVoltage,
    /// HID++ 1.0 设备：读电量寄存器 0x0D（真实百分比）。
    ReadHidpp1Register0x0D,
    /// HID++ 1.0 设备：读电量寄存器 0x07（粗略档位，0x0D 不可用时的回退）。
    ReadHidpp1Register0x07,
    /// HID Power Descriptor 回退：不在帧层编码，由传输层用 HID 特性报表读取。
    ReadHidPowerDescriptor,
}

impl QueryCommand {
    /// 编码为线上字节。`device_index` 为目标设备，`sw_id` 为请求配对 ID。
    ///
    /// 蓝图契约 `encode(&QueryCommand) -> Vec<u8>` 的落地形：feature_index
    /// 是每设备动态解析结果，必须随调用传入（见 docs/decisions.md D-02）。
    ///
    /// v0.2.0 协议实证修正（红队 #2/#3，蓝队 A2/A3/A4）：
    /// - HID++ 2.0 的所有 feature 调用一律走 **长帧**（0x11，20 字节）；
    /// - UNIFIED_BATTERY 的电量查询是 **fn1 getStatus**（fn0 是 GetCapabilities）；
    /// - HID++ 1.0 寄存器读必须经 **0x81 子地址**寻址：byte2=0x81，byte3=寄存器号
    ///   （寄存器号复用 sw_id 字段位，使传输层 sw_id 回显匹配对 1.0 响应同样成立）。
    pub fn encode_for(
        &self,
        device_index: u8,
        sw_id: u8,
        features: &FeatureTable,
    ) -> Result<Vec<u8>, HidppError> {
        // P1-1 双保险：只许查询类 feature 上线。
        fn require_query(feature_id: u16) -> Result<(), HidppError> {
            if registry().is_query(feature_id) {
                Ok(())
            } else {
                Err(HidppError::NotEncodable(
                    "feature is not query-kind (whitelist violation)",
                ))
            }
        }
        fn resolve(features: &FeatureTable, feature_id: u16) -> Result<u8, HidppError> {
            features.get(feature_id).ok_or(HidppError::NotEncodable(
                "feature index not resolved for device",
            ))
        }
        fn hidpp1_read(device_index: u8, reg: u8) -> Frame {
            // byte2=0x81（读子地址），byte3=寄存器号（复用 sw_id 位）。
            Frame::new_short(device_index, feature_ids::HIDPP1_READ_SUB, 0x00, reg, [0, 0, 0])
        }

        let frame = match self {
            QueryCommand::GetProtocolVersion => {
                require_query(feature_ids::IROOT)?;
                Frame::new_long(
                    device_index,
                    FeatureTable::iroot_index(),
                    0x01,
                    sw_id,
                    [0; 16],
                )
            }
            QueryCommand::GetFeatureIndex { feature_id } => {
                // HID++ 2.0：GetFeatureIndex 是 IRoot（恒 index 0）的 fn0，
                // 不依赖任何已解析 index（否则无法自举）。
                require_query(feature_ids::IROOT)?;
                require_query(*feature_id)?;
                Frame::new_long(
                    device_index,
                    FeatureTable::iroot_index(),
                    0x00,
                    sw_id,
                    [
                        (feature_id >> 8) as u8,
                        (feature_id & 0xFF) as u8,
                        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ],
                )
            }
            QueryCommand::GetBatteryUnified => {
                require_query(feature_ids::UNIFIED_BATTERY)?;
                let idx = resolve(features, feature_ids::UNIFIED_BATTERY)?;
                // fn1 getStatus（fn0 是 GetCapabilities，不返回电量）。
                Frame::new_long(device_index, idx, 0x01, sw_id, [0; 16])
            }
            QueryCommand::GetBatteryStatus => {
                require_query(feature_ids::BATTERY_STATUS)?;
                let idx = resolve(features, feature_ids::BATTERY_STATUS)?;
                Frame::new_long(device_index, idx, 0x00, sw_id, [0; 16])
            }
            QueryCommand::GetBatteryVoltage => {
                require_query(feature_ids::BATTERY_VOLTAGE)?;
                let idx = resolve(features, feature_ids::BATTERY_VOLTAGE)?;
                Frame::new_long(device_index, idx, 0x00, sw_id, [0; 16])
            }
            QueryCommand::ReadHidpp1Register0x0D => {
                hidpp1_read(device_index, feature_ids::HIDPP1_REG_BATTERY_CHARGE)
            }
            QueryCommand::ReadHidpp1Register0x07 => {
                hidpp1_read(device_index, feature_ids::HIDPP1_REG_BATTERY_STATUS)
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
        // 0x1004 fn1 getStatus，长帧（蓝队 A4 / 参考实现 feature() 实证）。
        let bytes = QueryCommand::GetBatteryUnified
            .encode_for(0x01, 0x07, &table())
            .expect("encode");
        assert_eq!(bytes[0], 0x11);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 0x06);
        assert_eq!(bytes[3], 0x17); // fn1 | sw 7
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn get_feature_index_encodes_via_iroot() {
        // IRoot 恒在 index 0，fn0；参数为 feature_id 大端；长帧。
        let bytes = QueryCommand::GetFeatureIndex { feature_id: 0x1004 }
            .encode_for(0x02, 0x03, &table())
            .expect("encode");
        assert_eq!(bytes[0], 0x11);
        assert_eq!(bytes[2], 0x00);
        assert_eq!(bytes[3], 0x03);
        assert_eq!(bytes[4], 0x10);
        assert_eq!(bytes[5], 0x04);
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn protocol_version_ping_is_long_frame() {
        let bytes = QueryCommand::GetProtocolVersion
            .encode_for(0x01, 0x05, &FeatureTable::new())
            .expect("encode");
        assert_eq!(bytes[0], 0x11);
        assert_eq!(bytes[2], 0x00);
        assert_eq!(bytes[3], 0x15); // fn1 | sw 5
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn hidpp1_registers_encode_with_0x81_sub_addressing() {
        // 参考实现实证：req = 0x8100 | reg → byte2=0x81，byte3=reg。
        let d = QueryCommand::ReadHidpp1Register0x0D
            .encode_for(0x02, 0x0A, &FeatureTable::new())
            .expect("encode");
        assert_eq!(d, vec![0x10, 0x02, 0x81, 0x0D, 0x00, 0x00, 0x00]);
        let s = QueryCommand::ReadHidpp1Register0x07
            .encode_for(0x02, 0x0A, &FeatureTable::new())
            .expect("encode");
        assert_eq!(s, vec![0x10, 0x02, 0x81, 0x07, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn unresolved_feature_is_not_encodable() {
        let t = FeatureTable::new();
        let err = QueryCommand::GetBatteryStatus
            .encode_for(0x01, 0x01, &t)
            .expect_err("must fail");
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
