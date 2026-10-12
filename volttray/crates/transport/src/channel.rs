//! HID 通道抽象：真实实现（hidapi，feature=hid）与测试桩都实现 [`HidChannel`]。

use std::fmt;
use std::time::Duration;

/// 通道 I/O 错误。`DeviceGone` 单独成变体：上层据此触发设备离线流程，
/// 且不进入写重试（重试无意义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoError {
    DeviceGone,
    Timeout,
    Other(String),
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceGone => write!(f, "device gone"),
            Self::Timeout => write!(f, "i/o timeout"),
            Self::Other(msg) => write!(f, "i/o error: {msg}"),
        }
    }
}

impl std::error::Error for IoError {}

/// 把底层错误文本归类（hidapi 没有结构化错误码，只能按消息匹配）。
pub fn classify_io(message: &str) -> IoError {
    let m = message.to_ascii_lowercase();
    if m.contains("disconnected") || m.contains("not connected") || m.contains("no such device") {
        IoError::DeviceGone
    } else if m.contains("timeout") || m.contains("timed out") {
        IoError::Timeout
    } else {
        IoError::Other(message.to_string())
    }
}

/// 字节通道：写一帧 / 读一帧（带超时）。
pub trait HidChannel {
    fn write(&mut self, data: &[u8]) -> Result<usize, IoError>;
    /// 返回读取字节数；超时返回 `Err(IoError::Timeout)`。
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, IoError>;
    /// 请求前清空通道里滞留的旧帧（参考实现实证：每次请求前排空最多 80 帧，
    /// 避免上一轮的迟到响应被当成本轮应答）。默认空操作，测试桩无需覆盖。
    fn drain(&mut self) {}
    /// HID 特性报表读取（HID Power Descriptor 回退路径用）。
    /// 默认不支持；真实 hidapi 通道覆盖此方法。
    fn get_feature_report(&mut self, _report_id: u8, _buf: &mut [u8]) -> Result<usize, IoError> {
        Err(IoError::Other(
            "feature report unsupported on this channel".into(),
        ))
    }
}
