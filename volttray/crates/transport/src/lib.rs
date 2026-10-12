//! volttray-transport —— HID 传输层。
//!
//! 职责（蓝图契约）：`request()` 一次请求-响应往返；读超时 1s；写失败重试一次
//! （`DeviceGone` 不重试）；响应按 device_index + sw_id 配对，错配帧跳过；
//! 观测事件经注入的 [`EventSink`] 上报（P1-2）。

pub mod channel;
pub mod events;
#[cfg(feature = "hid")]
pub mod hid_impl;

use channel::{HidChannel, IoError};
use events::{DiagEvent, DiagKind, EventSink, Layer};
use std::fmt;
use std::time::{Duration, Instant};
use volttray_protocol::frame::Frame;
use volttray_protocol::HidppError;

pub use channel::IoError as ChannelIoError;
pub use events::{Layer as EventLayer, NoopSink};

/// 读超时（参考实现实证：请求超时 0.4s；过长的超时会拖慢槽位扫描）。
pub const READ_TIMEOUT: Duration = Duration::from_millis(400);

/// HID Power Descriptor 回退尝试的 report id 候选（尽力而为，见 docs/decisions.md D-03）。
pub const HID_POWER_REPORT_IDS: [u8; 4] = [0x04, 0x05, 0x06, 0x07];

#[derive(Debug)]
pub enum TransportError {
    Protocol(HidppError),
    Io(IoError),
    Timeout,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Timeout => write!(f, "request timed out"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<HidppError> for TransportError {
    fn from(e: HidppError) -> Self {
        TransportError::Protocol(e)
    }
}

impl From<IoError> for TransportError {
    fn from(e: IoError) -> Self {
        TransportError::Io(e)
    }
}

pub struct Transport<C: HidChannel, S: EventSink> {
    ch: C,
    sink: S,
    next_sw: u8,
}

impl<C: HidChannel, S: EventSink> Transport<C, S> {
    pub fn new(ch: C, sink: S) -> Self {
        Transport {
            ch,
            sink,
            next_sw: 0,
        }
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// 分配 1..=15 的 sw_id（0 保留给设备自发消息）。
    pub fn alloc_sw_id(&mut self) -> u8 {
        self.next_sw = self.next_sw % 15 + 1;
        self.next_sw
    }

    fn emit(&mut self, kind: DiagKind) {
        self.sink.emit(DiagEvent::new(Layer::Transport, kind));
    }

    /// 一次请求-响应。`feature_id` 用于诊断事件与导出脱敏（P1-6）。
    ///
    /// v0.2.0 响应匹配修正（参考实现 `_xfer` 实证）：
    /// - 设备号匹配放宽为 `dev == req.dev || dev == req.dev ^ 0xFF`（蓝牙回显变体）；
    /// - 正常响应要求 byte2（feature/子地址）与 byte3 低位（sw_id / 寄存器号）**全回显**，
    ///   推送通知（sw_id=0 或非回显 feature）被自动滤除；
    /// - 错误帧仅按设备号匹配（参考实现同样不做回显校验）。
    pub fn request(
        &mut self,
        frame_bytes: &[u8],
        feature_id: Option<u16>,
    ) -> Result<Frame, TransportError> {
        let req = Frame::decode(frame_bytes)?;
        self.emit(DiagKind::FrameTx {
            hex: hex_of(frame_bytes),
            feature_id,
        });

        // 请求前排空滞留旧帧（真实通道覆盖 drain；测试桩默认空操作）。
        self.ch.drain();

        // 写：失败重试一次（DeviceGone 立即失败）。
        if let Err(e) = self.ch.write(frame_bytes) {
            if e == IoError::DeviceGone {
                self.emit(DiagKind::IoError {
                    message: e.to_string(),
                });
                return Err(TransportError::Io(e));
            }
            self.emit(DiagKind::Retry { attempt: 1 });
            self.ch.write(frame_bytes).map_err(|e2| {
                self.sink.emit(DiagEvent::new(
                    Layer::Transport,
                    DiagKind::IoError {
                        message: e2.to_string(),
                    },
                ));
                TransportError::Io(e2)
            })?;
        }

        // 读：超时窗口内跳过错配帧，直到配对成功或超时。
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut buf = [0u8; 64];
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.emit(DiagKind::Timeout {
                    after_ms: READ_TIMEOUT.as_millis() as u64,
                });
                return Err(TransportError::Timeout);
            }
            let remaining = deadline - now;
            match self.ch.read(&mut buf, remaining) {
                Ok(n) => {
                    let data = normalize_read(&buf[..n]);
                    match Frame::decode(&data) {
                        Ok(frame) => {
                            if response_matches(&req, &frame) {
                                self.emit(DiagKind::FrameRx {
                                    hex: hex_of(&data),
                                    feature_id,
                                });
                                return Ok(frame);
                            }
                            // 其它设备 / 其它请求的帧：跳过继续等。
                        }
                        Err(err @ HidppError::Device { device_index, .. })
                            if device_index == req.device_index
                                || device_index == req.device_index ^ 0xFF =>
                        {
                            self.emit(DiagKind::FrameRx {
                                hex: hex_of(&data),
                                feature_id,
                            });
                            return Err(TransportError::Protocol(err));
                        }
                        Err(HidppError::Device { .. }) => {
                            // 其它设备的错误帧：跳过。
                        }
                        Err(HidppError::Malformed(kind)) => {
                            self.emit(DiagKind::Info {
                                message: format!("malformed frame skipped: {kind:?}"),
                            });
                        }
                        Err(HidppError::NotEncodable(_)) => {
                            // decode 不会产生此错误；穷尽匹配，不静默。
                        }
                    }
                }
                Err(IoError::Timeout) => {
                    self.emit(DiagKind::Timeout {
                        after_ms: READ_TIMEOUT.as_millis() as u64,
                    });
                    return Err(TransportError::Timeout);
                }
                Err(e) => {
                    self.emit(DiagKind::IoError {
                        message: e.to_string(),
                    });
                    return Err(TransportError::Io(e));
                }
            }
        }
    }

    /// HID Power Descriptor 回退（尽力而为，真机未验证 —— D-03）。
    /// 返回 (percent, charging)；不可用返回 None。
    pub fn read_hid_power_descriptor(&mut self) -> Option<(u8, bool)> {
        let mut buf = [0u8; 64];
        for report_id in HID_POWER_REPORT_IDS {
            if let Ok(n) = self.ch.get_feature_report(report_id, &mut buf) {
                if n >= 3 && buf[1] <= 100 {
                    self.emit(DiagKind::Info {
                        message: format!("hid power descriptor via report 0x{report_id:02X}"),
                    });
                    return Some((buf[1], buf[2] & 0x01 == 0x01));
                }
            }
        }
        None
    }
}

/// hidapi 在不同平台对 report id 首字节的处理不一致（D-04）：
/// 若读回数据缺少 report id（6/19 字节），按长度补回。
fn normalize_read(raw: &[u8]) -> Vec<u8> {
    match raw.len() {
        6 if raw[0] != 0x10 => {
            let mut v = Vec::with_capacity(7);
            v.push(0x10);
            v.extend_from_slice(raw);
            v
        }
        19 if raw[0] != 0x11 => {
            let mut v = Vec::with_capacity(20);
            v.push(0x11);
            v.extend_from_slice(raw);
            v
        }
        _ => raw.to_vec(),
    }
}

/// 响应回显匹配：设备号（含蓝牙 `^0xFF` 变体）+ byte2（feature/子地址）+ byte3 低位
/// （sw_id，1.0 语义下即寄存器号）。推送通知（sw_id=0 / 非回显 feature）不匹配。
fn response_matches(req: &Frame, resp: &Frame) -> bool {
    let dev_hit = resp.device_index == req.device_index
        || resp.device_index == (req.device_index ^ 0xFF);
    dev_hit && resp.feature_index == req.feature_index && resp.sw_id == req.sw_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NoopSink;
    use std::collections::VecDeque;
    use volttray_protocol::frame::Frame;

    struct Queue {
        reads: VecDeque<Vec<u8>>,
    }
    impl crate::channel::HidChannel for Queue {
        fn write(&mut self, _d: &[u8]) -> Result<usize, IoError> {
            Ok(_d.len())
        }
        fn read(&mut self, buf: &mut [u8], _t: std::time::Duration) -> Result<usize, IoError> {
            match self.reads.pop_front() {
                Some(data) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                None => Err(IoError::Timeout),
            }
        }
    }

    fn tx(reads: Vec<Vec<u8>>) -> Transport<Queue, NoopSink> {
        Transport::new(
            Queue {
                reads: reads.into(),
            },
            NoopSink,
        )
    }

    /// 1.0 寄存器读：0x81 子地址 + 寄存器号回显匹配（根因 #3 回归）。
    #[test]
    fn hidpp1_register_roundtrip() {
        let mut t = tx(vec![vec![0x10, 0x02, 0x81, 0x0D, 0x5A, 0x00, 0x50]]);
        let req = [0x10, 0x02, 0x81, 0x0D, 0x00, 0x00, 0x00];
        let frame = t.request(&req, None).expect("matched");
        assert_eq!(frame.params(), &[0x5A, 0x00, 0x50]);
    }

    /// 推送通知（非回显 feature / sw_id=0）必须被滤除，不能顶替本轮应答。
    #[test]
    fn push_notifications_are_filtered() {
        let mut t = tx(vec![
            vec![0x10, 0x01, 0x41, 0x00, 0x0A, 0x00, 0x00], // 推送通知
            vec![
                0x11, 0x01, 0x06, 0x15, 0x50, 0x05, 0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0,
            ], // 真实应答
        ]);
        let req = [
            0x11, 0x01, 0x06, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let frame = t.request(&req, None).expect("matched after skip");
        assert_eq!(frame.params()[0], 0x50);
    }

    /// 蓝牙变体设备号（dev ^ 0xFF）也应答。
    #[test]
    fn bluetooth_dev_variant_matches() {
        let mut t = tx(vec![vec![
            0x11, 0x00, 0x00, 0x15, 0x02, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]]);
        let req = [
            0x11, 0xFF, 0x00, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(t.request(&req, None).is_ok());
    }

    /// 错误帧按设备号（含 ^0xFF 变体）匹配并转成协议错误。
    #[test]
    fn error_frame_matches_by_dev() {
        let mut t = tx(vec![vec![0x10, 0x02, 0x8F, 0x81, 0x0D, 0x08, 0x00]]);
        let req = [0x10, 0x02, 0x81, 0x0D, 0x00, 0x00, 0x00];
        match t.request(&req, None) {
            Err(TransportError::Protocol(HidppError::Device { code, .. })) => {
                assert_eq!(code, volttray_protocol::DeviceErrorCode::Busy);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn response_matcher_unit() {
        let req = Frame::new_long(0x01, 0x06, 0x01, 0x05, [0; 16]);
        let ok = Frame::new_long(0x01, 0x06, 0x01, 0x05, [0; 16]);
        let bt = Frame::new_long(0xFE, 0x06, 0x01, 0x05, [0; 16]);
        let wrong_feat = Frame::new_long(0x01, 0x05, 0x01, 0x05, [0; 16]);
        let push = Frame::new_long(0x01, 0x06, 0x00, 0x00, [0; 16]);
        assert!(response_matches(&req, &ok));
        assert!(response_matches(&req, &bt));
        assert!(!response_matches(&req, &wrong_feat));
        assert!(!response_matches(&req, &push));
    }
}

fn hex_of(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}
