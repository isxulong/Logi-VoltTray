//! 诊断观测事件（评审 P1-2）。
//!
//! 下层（protocol/transport/device）只通过注入的 [`EventSink`] 追加事件，
//! 事件的存储（RingBuffer）归 app crate 所有。`seq`/`ts_ms` 由 app 侧 sink
//! 在接收时盖戳，下层保持无时间依赖、可测试。

/// 事件来源层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Protocol,
    Transport,
    Device,
    App,
    Tray,
}

/// 事件负载。`feature_id` 供导出管线按 features.json 的 sensitive_params 脱敏（P1-6）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiagKind {
    FrameTx {
        hex: String,
        feature_id: Option<u16>,
    },
    FrameRx {
        hex: String,
        feature_id: Option<u16>,
    },
    IoError {
        message: String,
    },
    Retry {
        attempt: u8,
    },
    Timeout {
        after_ms: u64,
    },
    DeviceFound {
        id: String,
        name: String,
    },
    DeviceLost {
        id: String,
    },
    StateChange {
        id: String,
        from: String,
        to: String,
    },
    BatteryUpdate {
        id: String,
        percent: u8,
        charging: bool,
        source: String,
    },
    ConfigReloaded {
        warnings: Vec<String>,
    },
    Alert {
        level: String,
        message: String,
    },
    Info {
        message: String,
    },
}

/// 一条诊断事件。`seq` 与 `ts_ms` 由 app 侧 sink 盖戳。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiagEvent {
    pub seq: u64,
    pub ts_ms: u64,
    pub layer: Layer,
    #[serde(flatten)]
    pub kind: DiagKind,
}

impl DiagEvent {
    pub fn new(layer: Layer, kind: DiagKind) -> Self {
        DiagEvent {
            seq: 0,
            ts_ms: 0,
            layer,
            kind,
        }
    }
}

/// 事件接收口：下层仅持有此 trait 对象。
pub trait EventSink: Send {
    fn emit(&mut self, event: DiagEvent);
}

/// 丢弃所有事件的空实现（测试与降级路径用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&mut self, _event: DiagEvent) {}
}
