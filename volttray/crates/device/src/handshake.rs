//! 设备握手：协议版本探测 + 电量相关 feature index 解析（蓝图 6.2）。
//!
//! 流程：GetProtocolVersion ping（IRoot fn1，长帧）→ 按错误帧家族分类（红队差异 F5）：
//! - HID++ 1.0 错误帧（0x8F）且错误码非 0x01/0x08 → 真正的 1.0 设备（不认识 IRoot）；
//! - HID++ 1.0 错误码 0x01（ERR_UNKNOWN）→ 接收器幽灵槽特征，未证实的 1.0 候选
//!   （E10：继续按 1.0 链探测，但允许 0xFF 负缓存抑制其结构性失败）；
//! - HID++ 1.0 错误码 0x08（1.0 语义 UNKNOWN_DEVICE，槽位空置/设备休眠离线）→
//!   NoResponse(NoDevice)，上层喂死槽冷却、下轮重试（E10 细分，原为 Protocol）；
//! - HID++ 2.0 错误帧（0xFF）→ ping 阶段不正常（2.0 设备必须认识 IRoot），按 NoResponse
//!   处理留待重试，不得误判成 1.0 设备。
//!
//! 成功则逐个 GetFeatureIndex 解析 0x1004 / 0x1000 / 0x1001。超时按 NoResponse 处理。
//!
//! v0.3.11（F1）：无响应携带原因分类 [`NoResponseCause`]——此前 `Err(_)`
//! 同判 NoResponse，I/O 错误（缓存句柄可疑）与超时（设备休眠）在日志中
//! 不可区分，死句柄与深睡设备无法分辨。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::error::ErrorFamily;
use volttray_protocol::{FeatureTable, HidppError};
use volttray_transport::channel::HidChannel;
use volttray_transport::events::EventSink;
use volttray_transport::{Transport, TransportError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// HID++ 2.0 设备 + 已解析的 feature 表（可能为空表：该设备无电量 feature）。
    Hidpp2(FeatureTable),
    /// HID++ 1.0 设备（对 IRoot 报 1.0 协议错误，错误码非 0x01/0x08——
    /// D-17 红队实证的真实 1.0 设备应答形态）。
    Hidpp1,
    /// 未证实的 1.0 候选（E10）：1.0 族错误码 0x01（ERR_UNKNOWN）是接收器
    /// 幽灵槽（#255 伪槽）的应答特征，不是真实 1.0 设备的可信证据；仍按
    /// 1.0 寄存器链探测一次（万一是真设备保留注册通道），但其结构性失败
    /// 允许进 0xFF 负缓存——与可信 [`Handshake::Hidpp1`] 的缓存豁免区分。
    Hidpp1Ghost,
    /// 无响应（原因分类见 [`NoResponseCause`]，v0.3.11 F1）。
    NoResponse(NoResponseCause),
}

impl Handshake {
    /// 0xFF 负缓存是否适用于本握手结论（E10）：可信 HID++1 直连设备
    /// 不适用（AF-6：清缓存照常探测，防止真设备被长期误抑）；2.0 与
    /// 0x01 幽灵候选适用（结构性 error_response 在句柄存活期内确定性
    /// 复现，重试无益）。NoResponse 不进入探测阶段，返回 false 仅为
    /// 完备性，调用方不可达。
    pub fn ff_negative_cache_applies(&self) -> bool {
        match self {
            Handshake::Hidpp2(_) | Handshake::Hidpp1Ghost => true,
            Handshake::Hidpp1 | Handshake::NoResponse(_) => false,
        }
    }
}

/// 握手无响应的原因分类（v0.3.11，F1）：区分「设备不应答」与「句柄可疑」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoResponseCause {
    /// 读超时：设备休眠 / 离线 / 不应答。
    Timeout,
    /// I/O 错误（含设备消失）：缓存句柄可疑。
    Io,
    /// 协议层异常：2.0 ping 阶段错误帧 / 请求不可编码——设备（或接收器）
    /// 回了字节或本机编码失败，不是句柄死亡的直接证据，也不证明槽位空置。
    Protocol,
    /// 接收器应答「该槽位无设备」（E10：1.0 族错误码 0x08 UNKNOWN_DEVICE，
    /// 槽位空置 / 设备休眠离线）——句柄活着（接收器回了字节），但槽位
    /// 当前无设备应答，属死槽证据，喂死槽冷却裁决。
    NoDevice,
}

impl NoResponseCause {
    /// 是否喂入死槽冷却裁决（E10）：「槽位当前无设备应答」的两类证据——
    /// 纯超时不应答（直连通道深睡/空槽）与接收器 0x08 无设备应答——计入
    /// 连击；Io 证据保持热度留给 SlotScanStreak 裁决句柄死亡，Protocol
    /// （设备回了错误帧但语义异常）同样视为「槽位活着」，均不参与冷却。
    pub fn feeds_dead_slot_cooldown(self) -> bool {
        matches!(self, NoResponseCause::Timeout | NoResponseCause::NoDevice)
    }
}

/// 握手探测的 feature 全集（顺序即策略链顺序；0x0005 设备名 v0.3.0 新增）。
pub const PROBED_FEATURES: [u16; 5] = [
    feature_ids::UNIFIED_BATTERY,
    feature_ids::BATTERY_STATUS,
    feature_ids::BATTERY_VOLTAGE,
    feature_ids::DEVICE_NAME,
    // T6：DEVICE_FW_VERSION(0x0003) 仅注册期一次性探测（与既有 feature index
    // 探测同事务、不重试），用于提取 unitId 做物理设备身份去重；
    // 不进入任何周期轮询路径，电池红线内。
    feature_ids::DEVICE_FW_VERSION,
];

/// 把 IRoot ping 阶段的设备错误帧分类为握手结论（红队差异 F5 的实证规则；
/// E10 修订：0x01 不再判 Hidpp1，0x08 细分为 NoDevice）。
fn classify_ping_error(err: &HidppError) -> Handshake {
    let HidppError::Device { family, code, .. } = err else {
        return Handshake::NoResponse(NoResponseCause::Protocol);
    };
    match family {
        // 1.0 错误码 0x01 = ERR_UNKNOWN：接收器幽灵槽（#255 伪槽）的应答
        // 特征（真机实证：0xFF 槽 ping 每轮回 0x01、探测回 0x02），不是
        // 真实 1.0 设备的可信证据——按未证实 1.0 候选继续探测，允许负缓存。
        ErrorFamily::Hidpp1 if code.code() == 0x01 => Handshake::Hidpp1Ghost,
        // 1.0 错误码 0x08 = UNKNOWN_DEVICE：接收器应答「该槽位无设备」
        // （空槽 / 设备休眠离线）——句柄活着但槽位无设备，归 NoDevice
        // 喂死槽冷却（E10；此前归 Protocol 导致冷却永不喂入）。
        ErrorFamily::Hidpp1 if code.code() == 0x08 => {
            Handshake::NoResponse(NoResponseCause::NoDevice)
        }
        // 其余 1.0 错误 = 真实 1.0 设备证据（D-17 红队实证，保留不动）。
        ErrorFamily::Hidpp1 => Handshake::Hidpp1,
        // 2.0 错误帧：ping 阶段不正常（2.0 设备必须认识 IRoot），按
        // 无响应留待重试，不得误判成 1.0 设备。
        ErrorFamily::Hidpp2 => Handshake::NoResponse(NoResponseCause::Protocol),
    }
}

pub fn handshake<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_index: u8,
) -> Handshake {
    let sw = t.alloc_sw_id();
    let Ok(ping) =
        QueryCommand::GetProtocolVersion.encode_for(device_index, sw, &FeatureTable::new())
    else {
        return Handshake::NoResponse(NoResponseCause::Protocol);
    };
    // v0.3.11（F1）：超时与 I/O 错误分判——死句柄（Io）与深睡设备
    // （Timeout）向调用方透出不同类别，不再同判 NoResponse。
    match t.request(&ping, Some(feature_ids::IROOT)) {
        Ok(_) => {}
        Err(TransportError::Protocol(err @ HidppError::Device { .. })) => {
            return classify_ping_error(&err);
        }
        Err(TransportError::Protocol(_)) => {
            return Handshake::NoResponse(NoResponseCause::Protocol);
        }
        Err(TransportError::Timeout) => return Handshake::NoResponse(NoResponseCause::Timeout),
        Err(TransportError::Io(_)) => return Handshake::NoResponse(NoResponseCause::Io),
    }

    let mut table = FeatureTable::new();
    for fid in PROBED_FEATURES {
        let sw = t.alloc_sw_id();
        let Ok(req) =
            QueryCommand::GetFeatureIndex { feature_id: fid }.encode_for(device_index, sw, &table)
        else {
            continue;
        };
        if let Ok(frame) = t.request(&req, Some(fid)) {
            let index = frame.params().first().copied().unwrap_or(0);
            if index != 0 {
                table.insert(fid, index);
            }
        }
    }
    Handshake::Hidpp2(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;
    use volttray_transport::channel::IoError;
    use volttray_transport::events::DiagEvent;

    struct Stub {
        reads: VecDeque<Result<Vec<u8>, IoError>>,
    }
    impl HidChannel for Stub {
        fn write(&mut self, d: &[u8]) -> Result<usize, IoError> {
            Ok(d.len())
        }
        fn read(&mut self, buf: &mut [u8], _t: Duration) -> Result<usize, IoError> {
            match self.reads.pop_front().unwrap_or(Err(IoError::Timeout)) {
                Ok(data) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                Err(e) => Err(e),
            }
        }
    }
    struct Sink;
    impl EventSink for Sink {
        fn emit(&mut self, _e: DiagEvent) {}
    }

    fn stub(reads: Vec<Result<Vec<u8>, IoError>>) -> Transport<Stub, Sink> {
        Transport::new(
            Stub {
                reads: reads.into(),
            },
            Sink,
        )
    }

    /// 长帧响应（20 字节，0x11 前导），其余字节填 0。
    fn long_resp(dev: u8, feature: u8, func_sw: u8, p0: u8) -> Vec<u8> {
        let mut v = vec![0x11, dev, feature, func_sw, p0];
        v.resize(20, 0);
        v
    }

    #[test]
    fn hidpp2_resolves_feature_table() {
        let mut t = stub(vec![
            Ok(long_resp(0x01, 0x00, 0x11, 0x02)), // protocol 2.0
            Ok(long_resp(0x01, 0x00, 0x02, 0x06)), // 0x1004 → 6
            Ok(long_resp(0x01, 0x00, 0x03, 0x00)), // 0x1000 → 不支持(0)
            Ok(long_resp(0x01, 0x00, 0x04, 0x05)), // 0x1001 → 5
            Ok(long_resp(0x01, 0x00, 0x05, 0x03)), // 0x0005 → 3（设备名）
        ]);
        match handshake(&mut t, 0x01) {
            Handshake::Hidpp2(table) => {
                assert_eq!(table.get(0x1004), Some(6));
                assert_eq!(table.get(0x1000), None);
                assert_eq!(table.get(0x1001), Some(5));
                assert_eq!(table.get(0x0005), Some(3));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hidpp1_detected_on_1x8f_error() {
        // 1.0 错误帧 0x8F，错误码 0x07（非 0x01/0x08）→ 判 1.0 设备（D-17 实证保留）。
        let mut t = stub(vec![Ok(vec![0x10, 0x02, 0x8F, 0x00, 0x00, 0x07, 0x00])]);
        assert_eq!(handshake(&mut t, 0x02), Handshake::Hidpp1);
    }

    #[test]
    fn ghost_slot_code_0x01_is_unproven_hidpp1_and_cacheable() {
        // E10 回归（#255 负缓存永不生效的根修）：接收器幽灵槽对 IRoot ping
        // 回 1.0 族错误码 0x01（ERR_UNKNOWN）——不得再误判 Hidpp1（否则
        // try_slot 的 is_hidpp1 分支每轮清缓存，负缓存永不生效）；判为
        // 未证实 1.0 候选，继续按 1.0 寄存器链探测（真实设备仍保留注册
        // 通道），且其结构性失败允许进 0xFF 负缓存。
        let mut t = stub(vec![Ok(vec![0x10, 0xFF, 0x8F, 0x00, 0x00, 0x01, 0x00])]);
        let hs = handshake(&mut t, 0xFF);
        assert_eq!(hs, Handshake::Hidpp1Ghost, "0x01 不得误判 Hidpp1");
        assert!(
            hs.ff_negative_cache_applies(),
            "0x01 幽灵路径必须进入负缓存口径"
        );
    }

    #[test]
    fn credible_hidpp1_bypasses_negative_cache() {
        // AF-6 保护不回归：可信 HID++1 设备（0x07 等实证错误码）不适用
        // 负缓存——缓存被置位时清缓存照常探测，真设备不被长期误抑。
        let mut t = stub(vec![Ok(vec![0x10, 0xFF, 0x8F, 0x00, 0x00, 0x07, 0x00])]);
        let hs = handshake(&mut t, 0xFF);
        assert_eq!(hs, Handshake::Hidpp1);
        assert!(!hs.ff_negative_cache_applies());
    }

    #[test]
    fn hidpp1_sleeping_code_0x08_is_no_device() {
        // 1.0 错误码 0x08 = UNKNOWN_DEVICE（槽位空置/休眠/离线）：不得误判
        // 成 1.0 设备。E10：由 Protocol 细分为 NoDevice——接收器应答
        // 「槽位无设备」是死槽证据，喂死槽冷却；句柄活着（不回 Io）。
        let mut t = stub(vec![Ok(vec![0x10, 0x02, 0x8F, 0x00, 0x00, 0x08, 0x00])]);
        assert_eq!(
            handshake(&mut t, 0x02),
            Handshake::NoResponse(NoResponseCause::NoDevice)
        );
    }

    #[test]
    fn no_device_and_timeout_feed_dead_slot_cooldown() {
        // E10 回归（死槽冷却永不喂入的根修）：「槽位当前无设备应答」的
        // 两类证据（纯超时、接收器 0x08）喂冷却；Io（句柄可疑，留给
        // SlotScanStreak）与 Protocol（设备回了异常帧=槽位活着）不喂。
        assert!(NoResponseCause::Timeout.feeds_dead_slot_cooldown());
        assert!(NoResponseCause::NoDevice.feeds_dead_slot_cooldown());
        assert!(!NoResponseCause::Io.feeds_dead_slot_cooldown());
        assert!(!NoResponseCause::Protocol.feeds_dead_slot_cooldown());
    }

    #[test]
    fn hidpp2_error_frame_at_ping_is_no_response() {
        // 2.0 错误帧（0xFF）：ping 阶段出现即异常，按无响应重试，不判 1.0。
        let mut frame = vec![0x11, 0x02, 0xFF, 0x00, 0x11, 0x08];
        frame.resize(20, 0);
        let mut t = stub(vec![Ok(frame)]);
        assert_eq!(
            handshake(&mut t, 0x02),
            Handshake::NoResponse(NoResponseCause::Protocol)
        );
    }

    #[test]
    fn no_response_on_timeout() {
        // 读超时 = 设备休眠/离线/不应答（v0.3.11 F1：Timeout 类）。
        let mut t = stub(vec![]);
        assert_eq!(
            handshake(&mut t, 0x01),
            Handshake::NoResponse(NoResponseCause::Timeout)
        );
    }

    #[test]
    fn no_response_on_io_error() {
        // I/O 错误（如死句柄 read 报错）单独成类（v0.3.11 F1）：与超时
        // 可区分，供上层「扫槽连败」裁决把句柄可疑与设备深睡分开。
        let mut t = stub(vec![Err(IoError::Other("invalid handle".into()))]);
        assert_eq!(
            handshake(&mut t, 0x01),
            Handshake::NoResponse(NoResponseCause::Io)
        );
    }
}
