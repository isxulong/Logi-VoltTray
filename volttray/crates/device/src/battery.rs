//! 电量策略链（蓝图 6.3 + v0.2.0 协议实证修正）。
//!
//! 顺序：UNIFIED_BATTERY(0x1004) fn1 → BATTERY_STATUS(0x1000) fn0
//!       → BATTERY_VOLTAGE(0x1001) fn0 → HID Power Descriptor → 放弃。
//! HID++ 1.0 设备走寄存器 0x0D（真实百分比），0x0D 不可用回退 0x07（粗略档位）。
//!
//! 响应参数 → (percent, charging) 的映射以参考实现真机实证为准（红队差异 F2/F6、
//! 蓝队 B5-B9；详见 docs/decisions.md D-15）：
//! - unified fn1：[percent, level, state, extPower]，state∈{1,2}=充电、{1,2,3}=已插电
//! - status fn0：[percent, nextLevel, state]，同上 state 规则
//! - voltage fn0：[mv_hi, mv_lo, flags]，bit7=充电
//! - 1.0 0x0D：[level, ?, state]，state 高 4 bit：0x50/0x60=充电、0x90=已插满电
//! - 1.0 0x07：[roughLevel, chargingByte]，档位 {7:90,5:50,3:20,1:5}
//! - 百分比 >100 一律钳制到 100（不再判无效）
//!
//! 回退规则：设备错误帧（InvalidFeatureIndex / Unsupported / UnknownCode）与
//! 超时 → 下一策略（超时也降级：设备可能只支持后位 feature）；I/O 错误（含
//! DeviceGone）→ 中止并分类上报。
//! 全部失败返回 `Err(UnknownReason)`，诊断流记录 BatteryUnknown 事件（可定位）。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::error::{DeviceErrorCode, ErrorFamily, HidppError};
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
    Hidpp1Register0x0D,
    Hidpp1Register0x07,
    HidPowerDescriptor,
}

impl BatterySource {
    pub fn name(self) -> &'static str {
        match self {
            Self::UnifiedBattery => "unified_battery",
            Self::BatteryStatus => "battery_status",
            Self::BatteryVoltage => "battery_voltage",
            Self::Hidpp1Register0x0D => "hidpp1_register_0d",
            Self::Hidpp1Register0x07 => "hidpp1_register_0x07",
            Self::HidPowerDescriptor => "hid_power_descriptor",
        }
    }

    /// `name()` 的逆映射（WP-A 持久化读回：last_known.json 的 source 字段
    /// 还原为枚举）。未知名称返回 None——调用方按前向兼容处理（未来版本
    /// 新增的 source 在旧程序上降级为 None，不拒绝整条记录）。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "unified_battery" => Some(Self::UnifiedBattery),
            "battery_status" => Some(Self::BatteryStatus),
            "battery_voltage" => Some(Self::BatteryVoltage),
            "hidpp1_register_0d" => Some(Self::Hidpp1Register0x0D),
            "hidpp1_register_0x07" => Some(Self::Hidpp1Register0x07),
            "hid_power_descriptor" => Some(Self::HidPowerDescriptor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8,
    pub charging: bool,
    pub source: BatterySource,
}

/// 「电量未知」的原因分类（修复要求 B：让真机上的「？」可定位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnknownReason {
    /// 设备没有任何已解析的电量 feature（握手成功但全链路不可用）。
    NoBatteryFeatures,
    /// 请求超时（设备休眠 / 离线 / 不应答）。
    QueryTimeout,
    /// E22：HID++1 0x8F/0x08（UNKNOWN_DEVICE）——接收器应答但报告槽位上
    /// 无设备应答，真机上对应设备休眠/离线；语义等价于超时，走休眠路径。
    /// 仅限 HID++1 族：HID++2 0x08 是 Busy（设备在线繁忙），不落入此臂。
    NoDeviceAtSlot,
    /// 设备回了错误帧（带错误码）。
    ErrorResponse,
    /// 应答帧格式无法解析。
    MalformedResponse,
    /// 应答参数无法映射成电量（缺参数 / 未知档位等）。
    MappingFailure,
    /// 电压曲线缺失（按曲线名查不到）。
    CurveMissing,
    /// I/O 错误（非超时）。
    IoFailure,
    /// 设备已拔出。
    DeviceGone,
}

impl UnknownReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NoBatteryFeatures => "no_battery_features",
            Self::QueryTimeout => "query_timeout",
            Self::NoDeviceAtSlot => "no_device_at_slot",
            Self::ErrorResponse => "error_response",
            Self::MalformedResponse => "malformed_response",
            Self::MappingFailure => "mapping_failure",
            Self::CurveMissing => "curve_missing",
            Self::IoFailure => "io_error",
            Self::DeviceGone => "device_gone",
        }
    }
}

/// 百分比钳制：>100 → 100（参考实现 `_clamp_pct` 实证）。
pub fn clamp_pct(v: u8) -> u8 {
    v.min(100)
}

/// 映射：UNIFIED_BATTERY fn1 getStatus → [percent, level, state, extPower]。
/// state∈{1,2}=充电中、{1,2,3}=已插电；需要 ≥3 个参数。
pub fn map_unified_battery(params: &[u8]) -> Option<(u8, bool)> {
    if params.len() < 3 {
        return None;
    }
    let percent = clamp_pct(params[0]);
    let charging = params[2] == 1 || params[2] == 2;
    Some((percent, charging))
}

/// 映射：BATTERY_STATUS fn0 → [percent, nextLevel, state]，state 规则同 unified。
pub fn map_battery_status(params: &[u8]) -> Option<(u8, bool)> {
    if params.len() < 3 {
        return None;
    }
    let percent = clamp_pct(params[0]);
    let charging = params[2] == 1 || params[2] == 2;
    Some((percent, charging))
}

/// 电压映射的失败细分（供未知原因分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoltageMapError {
    BadParams,
    CurveMissing,
}

/// 映射：BATTERY_VOLTAGE fn0 → [mv_hi, mv_lo, flags]，bit7=充电中。
pub fn map_battery_voltage(params: &[u8], curve: &str) -> Result<(u8, bool), VoltageMapError> {
    if params.len() < 3 {
        return Err(VoltageMapError::BadParams);
    }
    let mv = u16::from_be_bytes([params[0], params[1]]);
    let Some(percent) = voltage::percent_from_mv(curve, mv) else {
        return Err(VoltageMapError::CurveMissing);
    };
    let charging = params[2] & 0x80 == 0x80;
    Ok((percent, charging))
}

/// 映射：HID++ 1.0 寄存器 0x0D → [level, ?, state]。
/// level>100 按 255 换算；state 高 4 bit：0x50/0x60=充电、0x90=已插电且 0% 视为 100。
pub fn map_hidpp1_register_0x0d(params: &[u8]) -> Option<(u8, bool)> {
    if params.len() < 3 {
        return None;
    }
    let raw = params[0];
    // 部分设备回 0..255 满量程：>100 时按 255 折算（参考实现实证）。
    let v = if raw > 100 {
        ((raw as u16 * 100 + 127) / 255) as u8
    } else {
        raw
    };
    let mut percent = clamp_pct(v);
    let st = params[2] & 0xF0;
    let charging = st == 0x50 || st == 0x60;
    if st == 0x90 && percent == 0 {
        percent = 100; // 已插电且读数为 0：充满
    }
    Some((percent, charging))
}

/// HID++ 1.0 粗略档位表（寄存器 0x07，参考实现 ROUGH_LEVEL 实证）。
pub const ROUGH_LEVEL: [(u8, u8); 4] = [(7, 90), (5, 50), (3, 20), (1, 5)];

/// 映射：HID++ 1.0 寄存器 0x07 → [roughLevel, chargingByte]。
/// chargingByte=0 放电；&0x22==0x22 已插电未充；&0x21==0x21 充电中。
pub fn map_hidpp1_register_0x07(params: &[u8]) -> Option<(u8, bool)> {
    if params.len() < 2 {
        return None;
    }
    let level_code = params[0];
    let percent = ROUGH_LEVEL
        .iter()
        .find(|(c, _)| *c == level_code)
        .map(|(_, p)| *p)?;
    let byte = params[1];
    let charging = byte & 0x21 == 0x21;
    Some((percent, charging))
}

/// 某策略失败后是否继续回退（true = 换下一策略）。
fn should_fallback(err: &TransportError) -> bool {
    match err {
        TransportError::Protocol(HidppError::Device { code, .. }) => matches!(
            code,
            DeviceErrorCode::InvalidFeatureIndex
                | DeviceErrorCode::Unsupported
                | DeviceErrorCode::UnknownCode(_)
        ),
        TransportError::Protocol(HidppError::Malformed(_)) => true,
        TransportError::Protocol(HidppError::NotEncodable(_)) => true,
        // v0.2.0：超时也降级到下一策略（设备可能只支持后位 feature；
        // 参考实现 refresh() 的链式兜底行为）。设备号拔出等 I/O 错误不降级。
        TransportError::Timeout => true,
        TransportError::Io(_) => false,
    }
}

/// `BatteryUnknown` 诊断事件的派发策略（F5 次修 P2，v0.3.16 线）。
/// 接收器直连槽 #255（0xFF）经握手误分类（D-17 规则不改）后，其
/// 0x0D 寄存器查询注定回协议错误——这类 `error_response` 是结构性
/// 噪声（每轮枚举必现），不是设备异常。调用方可对 #255 传
/// `SuppressReceiverSlotErrorResponse` 把事件压掉，改用 debug 级
/// 日志记录（tray 侧 bootlog），诊断导出不再被刷屏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownDiagPolicy {
    /// 照常派发 `BatteryUnknown` 诊断事件（默认，真实设备槽位）。
    Emit,
    /// 失败原因为 `ErrorResponse`（对端协议级错误响应）时跳过事件派发；
    /// 其他原因（超时 / I/O / 映射失败等）照常派发——只压结构性噪声，
    /// 不掩盖真实异常形态。
    SuppressReceiverSlotErrorResponse,
}

/// 按策略链探测一次电量。全部失败返回 `Err(未知原因分类)`（UI 显示"电量未知"）。
pub fn probe_battery<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_id: &str,
    device_index: u8,
    features: &FeatureTable,
    curve: &str,
    is_hidpp1: bool,
) -> Result<BatteryReading, UnknownReason> {
    probe_battery_with_policy(
        t,
        device_id,
        device_index,
        features,
        curve,
        is_hidpp1,
        UnknownDiagPolicy::Emit,
    )
}

/// `probe_battery` 的诊断策略变体（F5 次修 P2）：行为与默认完全一致，
/// 仅终态 `BatteryUnknown` 事件的派发受 `policy` 控制。
#[allow(clippy::too_many_arguments)]
pub fn probe_battery_with_policy<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_id: &str,
    device_index: u8,
    features: &FeatureTable,
    curve: &str,
    is_hidpp1: bool,
    policy: UnknownDiagPolicy,
) -> Result<BatteryReading, UnknownReason> {
    let emit = |t: &mut Transport<C, S>, kind: DiagKind| {
        t.sink_mut().emit(DiagEvent::new(Layer::Device, kind));
    };

    let strategies: &[(QueryCommand, BatterySource, Option<u16>)] = if is_hidpp1 {
        &[
            (
                QueryCommand::ReadHidpp1Register0x0D,
                BatterySource::Hidpp1Register0x0D,
                None,
            ),
            (
                QueryCommand::ReadHidpp1Register0x07,
                BatterySource::Hidpp1Register0x07,
                None,
            ),
        ]
    } else {
        &[
            (
                QueryCommand::GetBatteryUnified,
                BatterySource::UnifiedBattery,
                Some(feature_ids::UNIFIED_BATTERY),
            ),
            (
                QueryCommand::GetBatteryStatus,
                BatterySource::BatteryStatus,
                Some(feature_ids::BATTERY_STATUS),
            ),
            (
                QueryCommand::GetBatteryVoltage,
                BatterySource::BatteryVoltage,
                Some(feature_ids::BATTERY_VOLTAGE),
            ),
        ]
    };

    // 分类追踪：记录最后一次失败形态，供 BatteryUnknown 事件定位。
    let mut last_reason: Option<UnknownReason> = None;
    let mut last_detail = String::new();
    let mut any_encodable = false;

    for (cmd, source, fid) in strategies {
        let sw = t.alloc_sw_id();
        let Ok(bytes) = cmd.encode_for(device_index, sw, features) else {
            continue; // 未解析/不可编码 → 下一策略（典型：该 feature 不存在）
        };
        any_encodable = true;
        match t.request(&bytes, *fid) {
            Ok(frame) => {
                let mapped = match source {
                    BatterySource::UnifiedBattery => map_unified_battery(frame.params()),
                    BatterySource::BatteryStatus => map_battery_status(frame.params()),
                    BatterySource::BatteryVoltage => map_battery_voltage(frame.params(), curve)
                        .map_err(|e| {
                            last_reason = Some(match e {
                                VoltageMapError::CurveMissing => UnknownReason::CurveMissing,
                                VoltageMapError::BadParams => UnknownReason::MappingFailure,
                            });
                        })
                        .ok(),
                    BatterySource::Hidpp1Register0x0D => map_hidpp1_register_0x0d(frame.params()),
                    BatterySource::Hidpp1Register0x07 => map_hidpp1_register_0x07(frame.params()),
                    BatterySource::HidPowerDescriptor => None, // 不在此策略表内
                };
                if let Some((percent, charging)) = mapped {
                    let reading = BatteryReading {
                        percent,
                        charging,
                        source: *source,
                    };
                    emit(
                        t,
                        DiagKind::BatteryUpdate {
                            id: device_id.to_string(),
                            percent,
                            charging,
                            source: source.name().to_string(),
                        },
                    );
                    return Ok(reading);
                }
                // 参数映射失败：记录细分原因 → 下一策略。
                if last_reason.is_none() {
                    last_reason = Some(UnknownReason::MappingFailure);
                    last_detail = format!("params unmappable via {}", source.name());
                }
            }
            Err(err) => {
                let (reason, detail) = classify_transport_error(&err);
                if matches!(err, TransportError::Io(IoError::DeviceGone)) {
                    emit(
                        t,
                        DiagKind::DeviceLost {
                            id: device_id.to_string(),
                        },
                    );
                }
                last_reason = Some(reason);
                last_detail = detail;
                if !should_fallback(&err) {
                    // 中止：设备本身的问题，换策略无意义。
                    break;
                }
            }
        }
    }

    // HID Power Descriptor 兜底（尽力而为）。
    if let Some((percent, charging)) = t.read_hid_power_descriptor() {
        let reading = BatteryReading {
            percent,
            charging,
            source: BatterySource::HidPowerDescriptor,
        };
        emit(
            t,
            DiagKind::BatteryUpdate {
                id: device_id.to_string(),
                percent,
                charging,
                source: BatterySource::HidPowerDescriptor.name().to_string(),
            },
        );
        return Ok(reading);
    }

    let reason = match last_reason {
        Some(r) => r,
        None if !any_encodable => UnknownReason::NoBatteryFeatures,
        None => UnknownReason::QueryTimeout,
    };
    // F5 次修 P2：#255 结构性 error_response 噪声按策略压掉（tray 侧
    // 以 debug 级 bootlog 记录），其余形态照常派发——降级只针对
    // 「注定失败的误分类探测」，真实异常仍可观测。
    let suppressed = policy == UnknownDiagPolicy::SuppressReceiverSlotErrorResponse
        && reason == UnknownReason::ErrorResponse;
    if !suppressed {
        emit(
            t,
            DiagKind::BatteryUnknown {
                id: device_id.to_string(),
                reason: reason.code().to_string(),
                detail: last_detail,
            },
        );
    }
    Err(reason)
}

/// 传输错误 → 未知原因分类。
fn classify_transport_error(err: &TransportError) -> (UnknownReason, String) {
    match err {
        TransportError::Timeout => (UnknownReason::QueryTimeout, "no response in window".into()),
        TransportError::Io(e) if *e == IoError::DeviceGone => {
            (UnknownReason::DeviceGone, e.to_string())
        }
        TransportError::Io(e) => (UnknownReason::IoFailure, e.to_string()),
        // E22：HID++1 0x8F/0x08 = UNKNOWN_DEVICE（槽位休眠/离线），语义等价
        // 超时 → NoDeviceAtSlot 走休眠路径；必须按 family 区分——HID++2
        // 0x08 = Busy（在线繁忙），仍归 ErrorResponse，不得误判为休眠。
        TransportError::Protocol(HidppError::Device {
            family: ErrorFamily::Hidpp1,
            code,
            ..
        }) if code.code() == 0x08 => (
            UnknownReason::NoDeviceAtSlot,
            "device error ERR_BUSY (hidpp1 0x08 = UNKNOWN_DEVICE: slot asleep/offline)".into(),
        ),
        TransportError::Protocol(HidppError::Device { code, .. }) => (
            UnknownReason::ErrorResponse,
            format!("device error {}", code.name()),
        ),
        TransportError::Protocol(HidppError::Malformed(kind)) => (
            UnknownReason::MalformedResponse,
            format!("malformed: {kind:?}"),
        ),
        TransportError::Protocol(HidppError::NotEncodable(what)) => {
            (UnknownReason::NoBatteryFeatures, (*what).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_name_round_trip() {
        let all = [
            BatterySource::UnifiedBattery,
            BatterySource::BatteryStatus,
            BatterySource::BatteryVoltage,
            BatterySource::Hidpp1Register0x0D,
            BatterySource::Hidpp1Register0x07,
            BatterySource::HidPowerDescriptor,
        ];
        for s in all {
            assert_eq!(BatterySource::from_name(s.name()), Some(s));
        }
        // 未知名称（未来版本新增 / 手改文件）→ None，不 panic。
        assert_eq!(BatterySource::from_name("future_source"), None);
        assert_eq!(BatterySource::from_name(""), None);
    }

    #[test]
    fn hidpp1_unknown_device_maps_to_no_device_at_slot() {
        // E22：HID++1 0x8F/0x08 = UNKNOWN_DEVICE（槽位休眠/离线）→ 休眠路径。
        let err = TransportError::Protocol(HidppError::Device {
            device_index: 0x01,
            family: ErrorFamily::Hidpp1,
            feature_index: 0x0D,
            code: DeviceErrorCode::Busy, // 0x08，名称按 2.0 基线，语义看 family
        });
        let (reason, _) = classify_transport_error(&err);
        assert_eq!(reason, UnknownReason::NoDeviceAtSlot);
        // 不触发策略回退（与 Busy 一致：直接终止本轮查询）。
        assert!(!should_fallback(&err));
    }

    #[test]
    fn hidpp2_busy_stays_error_response() {
        // E22 安全核对：HID++2 0xFF/0x08 = Busy（在线繁忙）→ 不得误判休眠。
        let err = TransportError::Protocol(HidppError::Device {
            device_index: 0x01,
            family: ErrorFamily::Hidpp2,
            feature_index: 0x07,
            code: DeviceErrorCode::Busy,
        });
        let (reason, _) = classify_transport_error(&err);
        assert_eq!(reason, UnknownReason::ErrorResponse);
        assert!(!should_fallback(&err));
    }

    #[test]
    fn unified_mapping_matches_reference() {
        // [percent, level, state]：state=2 → 充电；state=4 → 非充电非插电（无效档）。
        assert_eq!(map_unified_battery(&[80, 2, 1, 0]), Some((80, true)));
        assert_eq!(map_unified_battery(&[80, 2, 2, 0]), Some((80, true)));
        assert_eq!(map_unified_battery(&[45, 3, 3, 1]), Some((45, false)));
        assert_eq!(map_unified_battery(&[45, 3, 4, 1]), Some((45, false)));
        // >100 钳制到 100（不再判无效）。
        assert_eq!(map_unified_battery(&[0xFF, 0, 1, 0]), Some((100, true)));
        // 参数不足 → None。
        assert_eq!(map_unified_battery(&[80, 2]), None);
    }

    #[test]
    fn status_mapping_matches_reference() {
        assert_eq!(map_battery_status(&[90, 2, 1]), Some((90, true)));
        assert_eq!(map_battery_status(&[50, 2, 3]), Some((50, false)));
        assert_eq!(map_battery_status(&[50, 2]), None);
    }

    #[test]
    fn voltage_mapping_bit7_charging() {
        // 0x105A = 4186mV（li-ion-logi 满电锚点）→ 100%；0x0F64=3940mV 实为 73%。
        assert_eq!(
            map_battery_voltage(&[0x10, 0x5A, 0x80], "li-ion-logi"),
            Ok((100, true))
        );
        assert_eq!(
            map_battery_voltage(&[0x10, 0x5A, 0x00], "li-ion-logi"),
            Ok((100, false))
        );
        assert_eq!(
            map_battery_voltage(&[0x0E, 0x62, 0x00], "no-such-curve"),
            Err(VoltageMapError::CurveMissing)
        );
        assert_eq!(
            map_battery_voltage(&[0x0F, 0x64], "li-ion-logi"),
            Err(VoltageMapError::BadParams)
        );
    }

    #[test]
    fn hidpp1_reg0d_mapping_matches_reference() {
        // 0x50/0x60 高 4 bit = 充电。
        assert_eq!(map_hidpp1_register_0x0d(&[70, 0, 0x50]), Some((70, true)));
        assert_eq!(map_hidpp1_register_0x0d(&[70, 0, 0x60]), Some((70, true)));
        assert_eq!(map_hidpp1_register_0x0d(&[70, 0, 0x30]), Some((70, false)));
        // 0x90 + 0% → 满电。
        assert_eq!(map_hidpp1_register_0x0d(&[0, 0, 0x90]), Some((100, false)));
        // >100 按 255 折算：255→100、200→78。
        assert_eq!(
            map_hidpp1_register_0x0d(&[255, 0, 0x00]),
            Some((100, false))
        );
        assert_eq!(map_hidpp1_register_0x0d(&[200, 0, 0x00]), Some((78, false)));
        assert_eq!(map_hidpp1_register_0x0d(&[70, 0]), None);
    }

    #[test]
    fn hidpp1_reg07_rough_levels() {
        assert_eq!(map_hidpp1_register_0x07(&[7, 0]), Some((90, false)));
        assert_eq!(map_hidpp1_register_0x07(&[5, 0x21]), Some((50, true)));
        assert_eq!(map_hidpp1_register_0x07(&[3, 0x22]), Some((20, false)));
        assert_eq!(map_hidpp1_register_0x07(&[1, 0x00]), Some((5, false)));
        // 未知档位 / 参数不足。
        assert_eq!(map_hidpp1_register_0x07(&[9, 0]), None);
        assert_eq!(map_hidpp1_register_0x07(&[7]), None);
    }

    #[test]
    fn clamp_caps_at_100() {
        assert_eq!(clamp_pct(100), 100);
        assert_eq!(clamp_pct(255), 100);
        assert_eq!(clamp_pct(7), 7);
    }
}
