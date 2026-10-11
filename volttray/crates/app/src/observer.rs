//! 诊断观测流（评审 P1-2 + P1-6）。
//!
//! - P1-2：RingBuffer<DiagEvent> 容量 ≥256 归 app 所有；下层经 EventSink 追加，
//!   事件序号与时间戳在本层盖戳（下层无时间依赖）。
//! - P1-6：导出管线按 features.json 的 sensitive_params 对帧 hex 做偏移遮蔽；
//!   回归测试断言导出文本中不含真实序列号。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use volttray_protocol::features::registry;
use volttray_transport::events::{DiagEvent, DiagKind, EventSink};

/// RingBuffer 容量（P1-2：≥256）。
pub const DIAG_CAPACITY: usize = 256;

/// 脱敏替换符（两字符，保持 hex 列对齐）。
pub const MASK: &str = "**";

#[derive(Debug)]
pub struct Observer {
    buf: VecDeque<DiagEvent>,
    next_seq: u64,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    pub fn new() -> Self {
        Observer { buf: VecDeque::with_capacity(DIAG_CAPACITY), next_seq: 1 }
    }

    /// 追加并盖戳（seq 单调递增，ts 为 epoch 毫秒）。
    pub fn push(&mut self, mut event: DiagEvent) {
        event.seq = self.next_seq;
        self.next_seq += 1;
        event.ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if self.buf.len() >= DIAG_CAPACITY {
            self.buf.pop_front();
        }
        self.buf.push_back(event);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn events(&self) -> impl Iterator<Item = &DiagEvent> {
        self.buf.iter()
    }

    /// 导出 JSON。`mask=true` 时对帧 hex 按 sensitive_params 遮蔽（P1-6）。
    pub fn export_json(&self, mask: bool) -> String {
        let events: Vec<DiagEvent> = self
            .buf
            .iter()
            .cloned()
            .map(|mut e| {
                if mask {
                    mask_event(&mut e);
                }
                e
            })
            .collect();
        serde_json::to_string_pretty(&events).unwrap_or_else(|_| "[]".to_string())
    }

    /// 渲染为人类可读文本（诊断面板用）。
    pub fn render_text(&self, mask: bool) -> String {
        let mut out = String::new();
        for e in self.buf.iter() {
            let mut e = e.clone();
            if mask {
                mask_event(&mut e);
            }
            out.push_str(&format!("#{:04} [{:?}] {:?}\n", e.seq, e.layer, e.kind));
        }
        out
    }
}

/// 对单条事件做帧级脱敏。
pub fn mask_event(event: &mut DiagEvent) {
    match &mut event.kind {
        DiagKind::FrameTx { hex, feature_id } | DiagKind::FrameRx { hex, feature_id } => {
            if let Some(fid) = feature_id {
                let offsets = registry().sensitive_offsets(*fid);
                if !offsets.is_empty() {
                    *hex = mask_hex(hex, offsets);
                }
            }
        }
        // 非帧事件无敏感字节，逐变体穷尽、不做处理。
        DiagKind::IoError { .. }
        | DiagKind::Retry { .. }
        | DiagKind::Timeout { .. }
        | DiagKind::DeviceFound { .. }
        | DiagKind::DeviceLost { .. }
        | DiagKind::StateChange { .. }
        | DiagKind::BatteryUpdate { .. }
        | DiagKind::ConfigReloaded { .. }
        | DiagKind::Alert { .. }
        | DiagKind::Info { .. } => {}
    }
}

/// 对空格分隔 hex 串按参数偏移遮蔽（帧布局：参数字节从第 4 字节起）。
pub fn mask_hex(hex: &str, param_offsets: &[u8]) -> String {
    let mut bytes: Vec<&str> = hex.split(' ').collect();
    for off in param_offsets {
        let idx = 4 + *off as usize;
        if idx < bytes.len() {
            bytes[idx] = MASK;
        }
    }
    bytes.join(" ")
}

/// 可共享的事件接收口：克隆后注入各下层 Transport / App。
#[derive(Clone)]
pub struct SharedObserver(pub Arc<Mutex<Observer>>);

impl SharedObserver {
    pub fn new() -> Self {
        SharedObserver(Arc::new(Mutex::new(Observer::new())))
    }
}

impl Default for SharedObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for SharedObserver {
    fn emit(&mut self, event: DiagEvent) {
        if let Ok(mut obs) = self.0.lock() {
            obs.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volttray_transport::events::Layer;

    fn frame_rx(serial_bytes: [u8; 4]) -> DiagEvent {
        // feat=0x07（DeviceSerialInfo，sensitive_params=[0,1,2,3]），参数含 "序列号"
        let hex = format!(
            "10 01 07 01 {:02X} {:02X} {:02X} {:02X}",
            serial_bytes[0], serial_bytes[1], serial_bytes[2], serial_bytes[3]
        );
        DiagEvent::new(Layer::Transport, DiagKind::FrameRx { hex, feature_id: Some(7) })
    }

    #[test]
    fn ring_buffer_caps_at_256() {
        let mut obs = Observer::new();
        for _ in 0..300 {
            obs.push(DiagEvent::new(Layer::App, DiagKind::Info { message: "x".into() }));
        }
        assert_eq!(obs.len(), 256);
        let first = obs.events().next().expect("first");
        assert_eq!(first.seq, 45); // 300 - 256 + 1
    }

    #[test]
    fn seq_stamps_monotonic() {
        let mut obs = Observer::new();
        obs.push(DiagEvent::new(Layer::App, DiagKind::Info { message: "a".into() }));
        obs.push(DiagEvent::new(Layer::App, DiagKind::Info { message: "b".into() }));
        let seqs: Vec<u64> = obs.events().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    /// P1-6 回归：导出文本中不得出现真实序列号字节。
    #[test]
    fn export_masks_serial_regression() {
        let serial = [0x12, 0x34, 0xAB, 0xCD];
        let mut obs = Observer::new();
        obs.push(frame_rx(serial));
        let masked = obs.export_json(true);
        assert!(!masked.contains("12 34 AB CD"), "serial leaked: {masked}");
        assert!(masked.contains("** ** ** **"), "mask missing: {masked}");
        let unmasked = obs.export_json(false);
        assert!(unmasked.contains("12 34 AB CD"), "unmasked export should keep raw bytes");
    }

    #[test]
    fn mask_hex_offsets() {
        let hex = "10 01 07 01 AA BB CC DD";
        assert_eq!(mask_hex(hex, &[0, 3]), "10 01 07 01 ** BB CC **");
        assert_eq!(mask_hex(hex, &[]), hex);
        assert_eq!(mask_hex(hex, &[99]), hex); // 越界偏移安全忽略
    }
}
