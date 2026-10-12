//! 传输层桩上测试：重试 / 超时 / 帧配对 / 错误传播 / 观测事件。

use std::collections::VecDeque;
use std::time::Duration;
use volttray_protocol::error::DeviceErrorCode;
use volttray_protocol::HidppError;
use volttray_transport::channel::{HidChannel, IoError};
use volttray_transport::events::{DiagEvent, DiagKind, EventSink};
use volttray_transport::{Transport, TransportError};

/// 脚本化通道：写结果队列 + 读结果队列。
struct StubChannel {
    writes: VecDeque<Result<usize, IoError>>,
    reads: VecDeque<Result<Vec<u8>, IoError>>,
    written: Vec<Vec<u8>>,
}

impl StubChannel {
    fn new() -> Self {
        StubChannel {
            writes: VecDeque::new(),
            reads: VecDeque::new(),
            written: Vec::new(),
        }
    }
    fn push_write(&mut self, r: Result<usize, IoError>) {
        self.writes.push_back(r);
    }
    fn push_read(&mut self, r: Result<Vec<u8>, IoError>) {
        self.reads.push_back(r);
    }
}

impl HidChannel for StubChannel {
    fn write(&mut self, data: &[u8]) -> Result<usize, IoError> {
        self.written.push(data.to_vec());
        self.writes.pop_front().unwrap_or(Ok(data.len()))
    }
    fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize, IoError> {
        match self.reads.pop_front().unwrap_or(Err(IoError::Timeout)) {
            Ok(data) => {
                buf[..data.len()].copy_from_slice(&data);
                Ok(data.len())
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Default)]
struct VecSink(Vec<DiagEvent>);

impl EventSink for VecSink {
    fn emit(&mut self, event: DiagEvent) {
        self.0.push(event);
    }
}

fn req_frame() -> Vec<u8> {
    vec![0x10, 0x01, 0x06, 0x07, 0, 0, 0] // dev=1 feat=6 func=0 sw=7
}

fn resp_frame() -> Vec<u8> {
    vec![0x10, 0x01, 0x06, 0x07, 0x50, 0x00, 0x01]
}

#[test]
fn happy_path_request_response() {
    let mut ch = StubChannel::new();
    ch.push_read(Ok(resp_frame()));
    let mut t = Transport::new(ch, VecSink::default());
    let frame = t.request(&req_frame(), Some(4100)).expect("request");
    assert_eq!(frame.params()[0], 0x50);
    let events = &t.sink_mut().0;
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, DiagKind::FrameTx { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, DiagKind::FrameRx { .. })));
}

#[test]
fn write_failure_retries_once() {
    let mut ch = StubChannel::new();
    ch.push_write(Err(IoError::Other("flaky".into())));
    ch.push_write(Ok(7));
    ch.push_read(Ok(resp_frame()));
    let mut t = Transport::new(ch, VecSink::default());
    let frame = t.request(&req_frame(), None).expect("request");
    assert_eq!(frame.params()[0], 0x50);
    assert!(t
        .sink_mut()
        .0
        .iter()
        .any(|e| matches!(e.kind, DiagKind::Retry { attempt: 1 })));
}

#[test]
fn device_gone_is_not_retried() {
    let mut ch = StubChannel::new();
    ch.push_write(Err(IoError::DeviceGone));
    ch.push_write(Ok(7));
    let mut t = Transport::new(ch, VecSink::default());
    let err = t.request(&req_frame(), None).expect_err("must fail");
    assert!(matches!(err, TransportError::Io(IoError::DeviceGone)));
    assert!(!t
        .sink_mut()
        .0
        .iter()
        .any(|e| matches!(e.kind, DiagKind::Retry { .. })));
}

#[test]
fn double_write_failure_propagates() {
    let mut ch = StubChannel::new();
    ch.push_write(Err(IoError::Other("a".into())));
    ch.push_write(Err(IoError::Other("b".into())));
    let mut t = Transport::new(ch, VecSink::default());
    let err = t.request(&req_frame(), None).expect_err("must fail");
    assert!(matches!(err, TransportError::Io(IoError::Other(_))));
}

#[test]
fn read_timeout_returns_timeout() {
    let ch = StubChannel::new(); // 默认全部 Timeout
    let mut t = Transport::new(ch, VecSink::default());
    let err = t.request(&req_frame(), None).expect_err("must fail");
    assert!(matches!(err, TransportError::Timeout));
    assert!(t
        .sink_mut()
        .0
        .iter()
        .any(|e| matches!(e.kind, DiagKind::Timeout { .. })));
}

#[test]
fn mismatched_frames_are_skipped() {
    let mut ch = StubChannel::new();
    ch.push_read(Ok(vec![0x10, 0x02, 0x06, 0x07, 1, 2, 3])); // 其它设备
    ch.push_read(Ok(vec![0x10, 0x01, 0x06, 0x09, 4, 5, 6])); // sw_id 错配
    ch.push_read(Ok(resp_frame()));
    let mut t = Transport::new(ch, VecSink::default());
    let frame = t.request(&req_frame(), None).expect("request");
    assert_eq!(frame.params()[0], 0x50);
}

#[test]
fn missing_report_id_is_normalized() {
    let mut ch = StubChannel::new();
    // 6 字节（hidapi 剥掉 report id 的情形）
    ch.push_read(Ok(vec![0x01, 0x06, 0x07, 0x50, 0x00, 0x01]));
    let mut t = Transport::new(ch, VecSink::default());
    let frame = t.request(&req_frame(), None).expect("request");
    assert_eq!(frame.params()[0], 0x50);
}

#[test]
fn device_error_frame_propagates_as_protocol_error() {
    let mut ch = StubChannel::new();
    // v0.2.0 错误帧布局：byte3=feat 回显(0x06) byte4=fn|sw(0x07) byte5=code(0x06)
    ch.push_read(Ok(vec![0x10, 0x01, 0xFF, 0x06, 0x07, 0x06, 0x00])); // ERR_INVALID_FEATURE_INDEX
    let mut t = Transport::new(ch, VecSink::default());
    let err = t.request(&req_frame(), None).expect_err("must fail");
    match err {
        TransportError::Protocol(HidppError::Device {
            code,
            feature_index,
            ..
        }) => {
            assert_eq!(code, DeviceErrorCode::InvalidFeatureIndex);
            assert_eq!(feature_index, 0x06);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn sw_id_allocation_cycles_1_to_15() {
    let ch = StubChannel::new();
    let mut t = Transport::new(ch, VecSink::default());
    let ids: Vec<u8> = (0..20).map(|_| t.alloc_sw_id()).collect();
    assert_eq!(&ids[..3], &[1, 2, 3]);
    assert!(ids.iter().all(|&i| (1..=15).contains(&i)));
}
