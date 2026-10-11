//! 电量策略链（蓝图 6.3）。
//!
//! 顺序：UNIFIED_BATTERY(0x1004) → BATTERY_STATUS(0x1000) → BATTERY_VOLTAGE(0x1001)
//!       → HID Power Descriptor → 放弃（"电量未知"，返回 None）。
//! HID++ 1.0 设备走寄存器 0x0D。
//! 响应参数 → (percent, charging) 的映射按 PRD 协议附录实现，
//! **真机未验证**（见 docs/decisions.md D-06）。
//!
//! 回退规则：设备错误帧（InvalidFeatureIndex / Unsupported / UnknownCode）→ 下一策略；
//! I/O 错误（DeviceGone / 其它）与超时 → 中止（设备本身的问题，换策略无意义）。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::error::{DeviceErrorCode, HidppError};
use volttray_protocol::voltage;
use volttray_protocol::FeatureTable;
use volttray_transport::channel::{HidChannel, IoError};
use volttray_transport::events::{DiagEvent, DiagKind, EventSink, Layer};
use volttray_transport::{Transport, TransportError};

/// 电量来源（诊断与 UI 展示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatterySource {
    UnifiedBattery,
    BatteryStatus,
    BatteryVoltage,
    Hidpp1Register,
    HidPowerDescriptor,
}

impl BatterySource {
    pub fn name(self) -> &'static str {
        match self {
            Self::UnifiedBattery => "unified_battery",
            Self::BatteryStatus => "battery_status",
            Self::BatteryVoltage => "battery_voltage",
            Self::Hidpp1Register => "hidpp1_register_0d",
            Self::HidPowerDescriptor => "hid_power_descriptor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8,
    pub charging: bool,
    pub source: BatterySource,
}

/// 映射：UNIFIED_BATTERY fn0 响应 → (percent, charging)。
/// params[0]=SOC 百分比，params[2] 低 2 bit = 状态（1=充电中）。（PRD 附录，D-06）
pub fn map_unified_battery(params: &[u8]) -> Option<(u8, bool)> {
    let percent = *params.first()?;
    if percent > 100 {
        return None;
    }
    let status = params.get(2).copied().unwrap_or(0);
    Some((percent, status & 0x03 == 0x01))
}

/// 映射：BATTERY_STATUS fn0 → params[0]=百分比，params[1] bit7=充电中。（PRD 附录，D-06）
pub fn map_battery_status(params: &[u8]) -> Option<(u8, bool)> {
    let percent = *params.first()?;
    if percent > 100 {
        return None;
    }
    let flags = params.get(1).copied().unwrap_or(0);
    Some((percent, flags & 0x80 == 0x80))
}

/// 映射：BATTERY_VOLTAGE fn0 → params[0..2]=毫伏（BE），按曲线换算。（PRD 附录）
pub fn map_battery_voltage(params: &[u8], curve: &str) -> Option<(u8, bool)> {
    if params.len() < 2 {
        return None;
    }
    let mv = u16::from_be_bytes([params[0], params[1]]);
    let percent = voltage::percent_from_mv(curve, mv)?;
    let charging = params.get(2).copied().unwrap_or(0) & 0x01 == 0x01;
    Some((percent, charging))
}

/// 映射：HID++ 1.0 寄存器 0x0D → params[0]=百分比，params[1] bit6=充电中。（PRD 附录，D-06）
pub fn map_hidpp1_register(params: &[u8]) -> Option<(u8, bool)> {
    let percent = *params.first()?;
    if percent > 100 {
        return None;
    }
    let flags = params.get(1).copied().unwrap_or(0);
    Some((percent, flags & 0x40 == 0x40))
}

/// 某策略是否值得继续回退（true = 换下一策略）。
fn should_fallback(err: &TransportError) -> bool {
    match err {
        TransportError::Protocol(HidppError::Device { code, .. }) => matches!(
            code,
            DeviceErrorCode::InvalidFeatureIndex | DeviceErrorCode::Unsupported | DeviceErrorCode::UnknownCode(_)
        ),
        TransportError::Protocol(HidppError::Malformed(_)) => true,
        TransportError::Protocol(HidppError::NotEncodable(_)) => true,
        TransportError::Io(_) | TransportError::Timeout => false,
    }
}

/// 按策略链探测一次电量。全部失败返回 None（UI 显示"电量未知"）。
pub fn probe_battery<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_id: &str,
    device_index: u8,
    features: &FeatureTable,
    curve: &str,
    is_hidpp1: bool,
) -> Option<BatteryReading> {
    let mut emit = |t: &mut Transport<C, S>, kind: DiagKind| {
        t.sink_mut().emit(DiagEvent::new(Layer::Device, kind));
    };

    let strategies: &[(QueryCommand, BatterySource, Option<u16>)] = if is_hidpp1 {
        &[(QueryCommand::ReadHidpp1Register0x0D, BatterySource::Hidpp1Register, None)]
    } else {
        &[
            (QueryCommand::GetBatteryUnified, BatterySource::UnifiedBattery, Some(feature_ids::UNIFIED_BATTERY)),
            (QueryCommand::GetBatteryStatus, BatterySource::BatteryStatus, Some(feature_ids::BATTERY_STATUS)),
            (QueryCommand::GetBatteryVoltage, BatterySource::BatteryVoltage, Some(feature_ids::BATTERY_VOLTAGE)),
        ]
    };

    for (cmd, source, fid) in strategies {
        let sw = t.alloc_sw_id();
        let Ok(bytes) = cmd.encode_for(device_index, sw, features) else {
            continue; // 未解析/不可编码 → 下一策略
        };
        match t.request(&bytes, *fid) {
            Ok(frame) => {
                let mapped = match source {
                    BatterySource::UnifiedBattery => map_unified_battery(frame.params()),
                    BatterySource::BatteryStatus => map_battery_status(frame.params()),
                    BatterySource::BatteryVoltage => map_battery_voltage(frame.params(), curve),
                    BatterySource::Hidpp1Register => map_hidpp1_register(frame.params()),
                    BatterySource::HidPowerDescriptor => None, // 不在此策略表内
                };
                if let Some((percent, charging)) = mapped {
                    let reading = BatteryReading { percent, charging, source: *source };
                    emit(t, DiagKind::BatteryUpdate {
                        id: device_id.to_string(),
                        percent,
                        charging,
                        source: source.name().to_string(),
                    });
                    return Some(reading);
                }
                // 参数映射失败（百分比>100 等）→ 下一策略
            }
            Err(err) => {
                if matches!(err, TransportError::Io(IoError::DeviceGone)) {
                    emit(t, DiagKind::DeviceLost { id: device_id.to_string() });
                }
                if !should_fallback(&err) {
                    emit(t, DiagKind::Info { message: format!("battery probe aborted: {err}") });
                    return None;
                }
            }
        }
    }

    // HID Power Descriptor 兜底（尽力而为）。
    if let Some((percent, charging)) = t.read_hid_power_descriptor() {
        let reading = BatteryReading { percent, charging, source: BatterySource::HidPowerDescriptor };
        emit(t, DiagKind::BatteryUpdate {
            id: device_id.to_string(),
            percent,
            charging,
            source: BatterySource::HidPowerDescriptor.name().to_string(),
        });
        return Some(reading);
    }

    emit(t, DiagKind::Info { message: format!("battery unknown for {device_id}") });
    None
}
