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

/// 读超时（蓝图：1s）。
pub const READ_TIMEOUT: Duration = Duration::from_millis(1000);

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
        Transport { ch, sink, next_sw: 0 }
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
    pub fn request(&mut self, frame_bytes: &[u8], feature_id: Option<u16>) -> Result<Frame, TransportError> {
        let req = Frame::decode(frame_bytes)?;
        self.emit(DiagKind::FrameTx { hex: hex_of(frame_bytes), feature_id });

        // 写：失败重试一次（DeviceGone 立即失败）。
        if let Err(e) = self.ch.write(frame_bytes) {
            if e == IoError::DeviceGone {
                self.emit(DiagKind::IoError { message: e.to_string() });
                return Err(TransportError::Io(e));
            }
            self.emit(DiagKind::Retry { attempt: 1 });
            self.ch.write(frame_bytes).map_err(|e2| {
                self.sink.emit(DiagEvent::new(Layer::Transport, DiagKind::IoError { message: e2.to_string() }));
                TransportError::Io(e2)
            })?;
        }

        // 读：1s 窗口内跳过错配帧，直到配对成功或超时。
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut buf = [0u8; 64];
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.emit(DiagKind::Timeout { after_ms: READ_TIMEOUT.as_millis() as u64 });
                return Err(TransportError::Timeout);
            }
            let remaining = deadline - now;
            match self.ch.read(&mut buf, remaining) {
                Ok(n) => {
                    let data = normalize_read(&buf[..n]);
                    match Frame::decode(&data) {
                        Ok(frame) => {
                            if frame.device_index == req.device_index && frame.sw_id == req.sw_id {
                                self.emit(DiagKind::FrameRx { hex: hex_of(&data), feature_id });
                                return Ok(frame);
                            }
                            // 其它设备 / 其它请求的帧：跳过继续等。
                        }
                        Err(err @ HidppError::Device { device_index, .. }) if device_index == req.device_index => {
                            self.emit(DiagKind::FrameRx { hex: hex_of(&data), feature_id });
                            return Err(TransportError::Protocol(err));
                        }
                        Err(HidppError::Device { .. }) => {
                            // 其它设备的错误帧：跳过。
                        }
                        Err(HidppError::Malformed(kind)) => {
                            self.emit(DiagKind::Info { message: format!("malformed frame skipped: {kind:?}") });
                        }
                        Err(HidppError::NotEncodable(_)) => {
                            // decode 不会产生此错误；穷尽匹配，不静默。
                        }
                    }
                }
                Err(IoError::Timeout) => {
                    self.emit(DiagKind::Timeout { after_ms: READ_TIMEOUT.as_millis() as u64 });
                    return Err(TransportError::Timeout);
                }
                Err(e) => {
                    self.emit(DiagKind::IoError { message: e.to_string() });
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

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}
