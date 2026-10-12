//! 设备名读取（v0.3.0「?」修复任务一）。
//!
//! 来源链（蓝队设备名实证条目 + 参考实现 read_name/receiver_device_name）：
//! 1. HID++ 2.0：DEVICE_NAME(0x0005) fn0 取总长 → fn1 按偏移分片读取，直到
//!    凑满总长（0 < 总长 ≤ 64）；分片按参考实现过滤 0x00 字节，分片短于
//!    槽宽即名字已提前结束；
//! 2. 回退（1.0 设备 / 2.0 feature 不可用）：接收器配对名寄存器 0x2B5，
//!    参数两变体（Unifying (0x40+dev-1, 0) / Bolt (0x60+dev, 1)），取首个
//!    通过名字校验（首字符字母数字，其后 2..=32 位合法字符）的应答；
//! 3. 均失败 → 返回 None，调用方使用干净回退名（如「USB Receiver · 槽位 1」），
//!    界面上不得出现裸「?」。
//!
//! 名字在 `Device::new` 处统一消毒（P1-5），本模块只负责取原始串。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::FeatureTable;
use volttray_transport::channel::HidChannel;
use volttray_transport::events::EventSink;
use volttray_transport::{Transport, TransportError};

/// fn1 单分片可携带的最大名字字节数（长帧参数区 16 字节）。
const CHUNK_CAP: usize = 16;
/// 接受的最大名字总长（fn0 返回值上限，与消毒上限对齐）。
const MAX_NAME_LEN: usize = 64;

/// 参考实现 `_NAME_OK`（^`[A-Za-z0-9][A-Za-z0-9 _\-\.\+/()]{1,31}$`）。
fn name_ok(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    if rest.is_empty() || rest.len() > 31 {
        return false;
    }
    rest.iter().all(|&c| {
        c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.' | '+' | '/' | '(' | ')')
    })
}

/// 应答参数 → 名字（NUL 截断）。
fn params_to_name(params: &[u8]) -> String {
    let end = params.iter().position(|&b| b == 0).unwrap_or(params.len());
    String::from_utf8_lossy(&params[..end]).trim().to_string()
}

/// HID++ 2.0：DEVICE_NAME fn0 总长 + fn1 分片读取。
fn read_name_via_feature<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_index: u8,
    features: &FeatureTable,
) -> Option<String> {
    features.get(feature_ids::DEVICE_NAME)?;
    let sw = t.alloc_sw_id();
    let req = QueryCommand::GetDeviceNameLength
        .encode_for(device_index, sw, features)
        .ok()?;
    let frame = t.request(&req, Some(feature_ids::DEVICE_NAME)).ok()?;
    let total = *frame.params().first()? as usize;
    if total == 0 || total > MAX_NAME_LEN {
        return None; // 0 = 无名；>64 = 异常值，不信任
    }
    let mut out: Vec<u8> = Vec::with_capacity(total);
    while out.len() < total {
        let sw = t.alloc_sw_id();
        let req = QueryCommand::GetDeviceNameChunk {
            offset: out.len() as u8,
        }
        .encode_for(device_index, sw, features)
        .ok()?;
        let frame = t.request(&req, Some(feature_ids::DEVICE_NAME)).ok()?;
        let frag = frame.params();
        // 参考实现实证：过滤 0x00；全 0 分片即结束（设备不再有内容）。
        let piece: Vec<u8> = frag.iter().copied().filter(|&b| b != 0).collect();
        if piece.is_empty() {
            break;
        }
        let take = (total - out.len()).min(piece.len());
        out.extend_from_slice(&piece[..take]);
        if piece.len() < CHUNK_CAP {
            break; // 分片未满槽：名字已提前结束
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&out).trim().to_string())
}

/// 接收器配对名寄存器 0x2B5（dev 1..=6；两参数变体都试）。
fn read_name_via_receiver<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_index: u8,
) -> Option<String> {
    if !(1..=6).contains(&device_index) {
        return None;
    }
    let variants = [
        QueryCommand::ReadReceiverRegister0x2B5 {
            p0: 0x40 + device_index - 1,
            p1: 0,
        },
        QueryCommand::ReadReceiverRegister0x2B5 {
            p0: 0x60 + device_index,
            p1: 1,
        },
    ];
    for cmd in variants {
        let req = cmd.encode_for(0xFF, 0x05, &FeatureTable::new()).ok()?;
        match t.request(&req, Some(feature_ids::HIDPP1_REG_RECEIVER_INFO)) {
            Ok(frame) => {
                let name = params_to_name(frame.params());
                if name_ok(&name) {
                    return Some(name);
                }
            }
            Err(TransportError::Timeout) | Err(TransportError::Protocol(_)) => continue,
            Err(TransportError::Io(_)) => return None, // 通道级故障：不再试
        }
    }
    None
}

/// 读取设备显示名（未消毒的原始串）；全链失败返回 None。
pub fn read_device_name<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_index: u8,
    features: &FeatureTable,
    is_hidpp1: bool,
) -> Option<String> {
    if !is_hidpp1 {
        if let Some(name) = read_name_via_feature(t, device_index, features) {
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    read_name_via_receiver(t, device_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::Duration;
    use volttray_transport::channel::IoError;
    use volttray_transport::events::DiagEvent;

    struct Stub {
        reads: VecDeque<Result<Vec<u8>, IoError>>,
        writes: Rc<RefCell<Vec<Vec<u8>>>>,
    }
    impl HidChannel for Stub {
        fn write(&mut self, d: &[u8]) -> Result<usize, IoError> {
            self.writes.borrow_mut().push(d.to_vec());
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

    type Writes = Rc<RefCell<Vec<Vec<u8>>>>;
    type StubTransport = Transport<Stub, Sink>;

    fn stub(reads: Vec<Result<Vec<u8>, IoError>>) -> (StubTransport, Writes) {
        let writes: Writes = Rc::new(RefCell::new(Vec::new()));
        let t = Transport::new(
            Stub {
                reads: reads.into(),
                writes: writes.clone(),
            },
            Sink,
        );
        (t, writes)
    }

    fn feature_table() -> FeatureTable {
        let mut t = FeatureTable::new();
        t.insert(feature_ids::DEVICE_NAME, 0x03);
        t
    }

    #[test]
    fn hidpp2_chunked_name_read() {
        // 总长 11（fn0 sw=1 → byte3=0x01）；分片 "MX Master 3"（fn1 sw=2 → byte3=0x12）。
        // 分片 11 字节 < 16 槽宽 → 提前结束。
        let (mut t, _writes) = stub(vec![
            Ok(vec![0x10, 0x01, 0x03, 0x01, 0x0B, 0x00, 0x00]),
            Ok(vec![
                0x11, 0x01, 0x03, 0x12, b'M', b'X', b' ', b'M', b'a', b's', b't', b'e', b'r', b' ',
                b'3', 0, 0, 0, 0, 0,
            ]),
        ]);
        let name = read_device_name(&mut t, 0x01, &feature_table(), false);
        assert_eq!(name.as_deref(), Some("MX Master 3"));
    }

    #[test]
    fn hidpp2_full_chunk_needs_second_read() {
        // 总长 20 > 16：第一分片满 16 字节（无 0x00），第二分片凑尾 4 字节。
        let frag0 = b"ABCDEFGHIJKLMNOP".to_vec();
        let mut first = vec![0x11, 0x01, 0x03, 0x12];
        first.extend_from_slice(&frag0);
        let frag1 = b"QRST".to_vec();
        let mut second = vec![0x11, 0x01, 0x03, 0x13];
        second.extend_from_slice(&frag1);
        // 长帧必须恰 20 字节：不足会 BadLength 被跳帧，桩空后 Timeout → None。
        second.extend(std::iter::repeat_n(0u8, 16 - frag1.len()));
        let (mut t, writes) = stub(vec![
            Ok(vec![0x10, 0x01, 0x03, 0x01, 0x14, 0x00, 0x00]),
            Ok(first),
            Ok(second),
        ]);
        let name = read_device_name(&mut t, 0x01, &feature_table(), false);
        assert_eq!(name.as_deref(), Some("ABCDEFGHIJKLMNOPQRST"));
        // 第二次分片请求偏移 = 16。
        assert_eq!(writes.borrow()[2][4], 16);
    }

    #[test]
    fn hidpp2_zero_length_or_unresolved_returns_none() {
        // fn0 总长 0 → None（随后 0x2B5 对 dev1 也超时 → 全链 None）。
        let (mut t, _writes) = stub(vec![
            Ok(vec![0x10, 0x01, 0x03, 0x01, 0x00, 0x00, 0x00]),
            Err(IoError::Timeout),
            Err(IoError::Timeout),
        ]);
        assert_eq!(
            read_device_name(&mut t, 0x01, &feature_table(), false),
            None
        );
        // 未解析 DEVICE_NAME（空表）→ 直接走 0x2B5 回退。
        let (mut t2, _w2) = stub(vec![Err(IoError::Timeout), Err(IoError::Timeout)]);
        assert_eq!(
            read_device_name(&mut t2, 0x01, &FeatureTable::new(), false),
            None
        );
    }

    #[test]
    fn receiver_register_0x2b5_name() {
        // 黄金样例回放：dev2 Unifying 变体 p0=0x41 → "MX Keys"。
        let (mut t, writes) = stub(vec![Ok(vec![
            0x11, 0xFF, 0x83, 0xB5, b'M', b'X', b' ', b'K', b'e', b'y', b's', 0, 0, 0, 0, 0, 0, 0,
            0, 0,
        ])]);
        assert_eq!(
            read_device_name(&mut t, 0x02, &FeatureTable::new(), true).as_deref(),
            Some("MX Keys")
        );
        // 请求字节：byte2=0x83、byte3=0xB5、p0=0x41、p1=0x00。
        let ws = writes.borrow();
        assert_eq!(&ws[0][..6], &[0x11, 0xFF, 0x83, 0xB5, 0x41, 0x00]);
    }

    #[test]
    fn receiver_register_tries_both_variants_then_gives_up() {
        // Unifying 变体超时 → Bolt 变体 (p0=0x62, p1=1) 命中 "MX Ergo"。
        let (mut t, writes) = stub(vec![
            Err(IoError::Timeout),
            Ok(vec![
                0x11, 0xFF, 0x83, 0xB5, b'M', b'X', b' ', b'E', b'r', b'g', b'o', 0, 0, 0, 0, 0, 0,
                0, 0, 0,
            ]),
        ]);
        assert_eq!(
            read_device_name(&mut t, 0x02, &FeatureTable::new(), true).as_deref(),
            Some("MX Ergo")
        );
        assert_eq!(&writes.borrow()[1][4..6], &[0x62, 0x01]);
        // dev=7 超出配对范围 → 不发任何请求。
        let (mut t2, w2) = stub(vec![]);
        assert_eq!(
            read_device_name(&mut t2, 0x07, &FeatureTable::new(), true),
            None
        );
        assert!(w2.borrow().is_empty());
    }

    #[test]
    fn invalid_receiver_name_rejected() {
        // 名字不通过 name_ok（首字符非字母数字）→ 放弃，返回 None。
        let (mut t, _writes) = stub(vec![Ok(vec![
            0x11, 0xFF, 0x83, 0xB5, b' ', b'?', b'!', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])]);
        assert_eq!(
            read_device_name(&mut t, 0x02, &FeatureTable::new(), true),
            None
        );
    }

    #[test]
    fn name_ok_matches_reference_regex() {
        assert!(name_ok("MX Keys"));
        assert!(name_ok("MX Master 3S"));
        assert!(name_ok("A-1 (Wireless)"));
        assert!(!name_ok(""));
        assert!(!name_ok(" MX"));
        assert!(!name_ok("<bad>"));
        assert!(!name_ok("X")); // 参考实现要求总长 ≥ 2
    }
}
