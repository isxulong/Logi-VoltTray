//! 设备握手：协议版本探测 + 电量相关 feature index 解析（蓝图 6.2）。
//!
//! 流程：GetProtocolVersion ping（IRoot fn1，长帧）→ 按错误帧家族分类（红队差异 F5）：
//! - HID++ 1.0 错误帧（0x8F）且错误码非 0x08 → 真正的 1.0 设备（不认识 IRoot）；
//! - HID++ 1.0 错误帧且错误码 0x08（1.0 语义 UNKNOWN_DEVICE，设备休眠/离线）→ NoResponse，
//!   上层置 Sleeping，下轮重试；
//! - HID++ 2.0 错误帧（0xFF）→ ping 阶段不正常（2.0 设备必须认识 IRoot），按 NoResponse
//!   处理留待重试，不得误判成 1.0 设备。
//!
//! 成功则逐个 GetFeatureIndex 解析 0x1004 / 0x1000 / 0x1001。超时按 NoResponse 处理。

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
    /// HID++ 1.0 设备（对 IRoot 报 1.0 协议错误，错误码非 0x08）。
    Hidpp1,
    /// 无响应（超时 / I/O 错误 / 1.0 休眠 0x08 / 2.0 ping 阶段异常错误帧）。
    NoResponse,
}

/// 握手探测的电量 feature 全集（顺序即策略链顺序）。
pub const PROBED_FEATURES: [u16; 3] = [
    feature_ids::UNIFIED_BATTERY,
    feature_ids::BATTERY_STATUS,
    feature_ids::BATTERY_VOLTAGE,
];

/// 把 IRoot ping 阶段的设备错误帧分类为握手结论（红队差异 F5 的实证规则）。
fn classify_ping_error(err: &HidppError) -> Handshake {
    let HidppError::Device { family, code, .. } = err else {
        return Handshake::NoResponse;
    };
    match family {
        // 1.0 错误码 0x08 = UNKNOWN_DEVICE：设备休眠/离线，不是 1.0 协议的判据。
        ErrorFamily::Hidpp1 if code.code() != 0x08 => Handshake::Hidpp1,
        // 1.0 0x08（休眠）与 2.0 错误帧都按"本轮无响应"处理，留待下轮重试。
        ErrorFamily::Hidpp1 | ErrorFamily::Hidpp2 => Handshake::NoResponse,
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
        return Handshake::NoResponse;
    };
    match t.request(&ping, Some(feature_ids::IROOT)) {
        Ok(_) => {}
        Err(TransportError::Protocol(err @ HidppError::Device { .. })) => {
            return classify_ping_error(&err);
        }
        Err(_) => return Handshake::NoResponse,
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
        ]);
        match handshake(&mut t, 0x01) {
            Handshake::Hidpp2(table) => {
                assert_eq!(table.get(0x1004), Some(6));
                assert_eq!(table.get(0x1000), None);
                assert_eq!(table.get(0x1001), Some(5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hidpp1_detected_on_1x8f_error() {
        // 1.0 错误帧 0x8F，错误码 0x07（非 0x08）→ 判 1.0 设备。
        let mut t = stub(vec![Ok(vec![0x10, 0x02, 0x8F, 0x00, 0x00, 0x07, 0x00])]);
        assert_eq!(handshake(&mut t, 0x02), Handshake::Hidpp1);
    }

    #[test]
    fn hidpp1_sleeping_code_0x08_is_no_response() {
        // 1.0 错误码 0x08 = UNKNOWN_DEVICE（休眠/离线）：不得误判成 1.0 设备。
        let mut t = stub(vec![Ok(vec![0x10, 0x02, 0x8F, 0x00, 0x00, 0x08, 0x00])]);
        assert_eq!(handshake(&mut t, 0x02), Handshake::NoResponse);
    }

    #[test]
    fn hidpp2_error_frame_at_ping_is_no_response() {
        // 2.0 错误帧（0xFF）：ping 阶段出现即异常，按无响应重试，不判 1.0。
        let mut frame = vec![0x11, 0x02, 0xFF, 0x00, 0x11, 0x08];
        frame.resize(20, 0);
        let mut t = stub(vec![Ok(frame)]);
        assert_eq!(handshake(&mut t, 0x02), Handshake::NoResponse);
    }

    #[test]
    fn no_response_on_timeout() {
        let mut t = stub(vec![]);
        assert_eq!(handshake(&mut t, 0x01), Handshake::NoResponse);
    }
}
