//! 设备握手：协议版本探测 + 电量相关 feature index 解析（蓝图 6.2）。
//!
//! 流程：GetProtocolVersion（IRoot fn1）→ 报错说明是 HID++ 1.0 设备；
//! 成功则逐个 GetFeatureIndex 解析 0x1004 / 0x1000 / 0x1001。
//! 超时按 NoResponse 处理（上层把设备置 Sleeping，下轮重试）。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::FeatureTable;
use volttray_protocol::HidppError;
use volttray_transport::channel::HidChannel;
use volttray_transport::events::EventSink;
use volttray_transport::{Transport, TransportError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// HID++ 2.0 设备 + 已解析的 feature 表（可能为空表：该设备无电量 feature）。
    Hidpp2(FeatureTable),
    /// HID++ 1.0 设备（对 IRoot 报协议错误）。
    Hidpp1,
    /// 无响应（超时 / I/O 错误）。
    NoResponse,
}

/// 握手探测的电量 feature 全集（顺序即策略链顺序）。
pub const PROBED_FEATURES: [u16; 3] = [
    feature_ids::UNIFIED_BATTERY,
    feature_ids::BATTERY_STATUS,
    feature_ids::BATTERY_VOLTAGE,
];

pub fn handshake<C: HidChannel, S: EventSink>(t: &mut Transport<C, S>, device_index: u8) -> Handshake {
    let sw = t.alloc_sw_id();
    let Ok(ping) = QueryCommand::GetProtocolVersion.encode_for(device_index, sw, &FeatureTable::new()) else {
        return Handshake::NoResponse;
    };
    match t.request(&ping, Some(feature_ids::IROOT)) {
        Ok(_) => {}
        Err(TransportError::Protocol(HidppError::Device { .. })) => return Handshake::Hidpp1,
        Err(_) => return Handshake::NoResponse,
    }

    let mut table = FeatureTable::new();
    for fid in PROBED_FEATURES {
        let sw = t.alloc_sw_id();
        let Ok(req) = QueryCommand::GetFeatureIndex { feature_id: fid }.encode_for(device_index, sw, &table) else {
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
        Transport::new(Stub { reads: reads.into() }, Sink)
    }

    #[test]
    fn hidpp2_resolves_feature_table() {
        let mut t = stub(vec![
            Ok(vec![0x10, 0x01, 0x00, 0x11, 0x02, 0x00, 0x00]), // protocol 2.0
            Ok(vec![0x10, 0x01, 0x00, 0x02, 0x06, 0x00, 0x00]), // 0x1004 → 6
            Ok(vec![0x10, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00]), // 0x1000 → 不支持(0)
            Ok(vec![0x10, 0x01, 0x00, 0x04, 0x05, 0x00, 0x00]), // 0x1001 → 5
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
    fn hidpp1_detected_on_device_error() {
        let mut t = stub(vec![Ok(vec![0x10, 0x02, 0xFF, 0x01, 0x00, 0x07, 0x00])]); // ERR_INVALID_FUNCTION_ID
        assert_eq!(handshake(&mut t, 0x02), Handshake::Hidpp1);
    }

    #[test]
    fn no_response_on_timeout() {
        let mut t = stub(vec![]);
        assert_eq!(handshake(&mut t, 0x01), Handshake::NoResponse);
    }
}
